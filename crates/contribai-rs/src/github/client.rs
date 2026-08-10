//! Async GitHub REST API client.
//!
//! Handles all GitHub REST API interactions: repo metadata,
//! file content, forking, branching, committing, and PR creation.
//! Direct port from Python `github/client.py` — httpx → reqwest.

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

use crate::core::error::{ContribError, Result};
use crate::core::models::{FileNode, Issue, Repository};

use std::sync::atomic::{AtomicU32, Ordering};

const GITHUB_API: &str = "https://api.github.com";
const MAX_REQUEST_ATTEMPTS: u32 = 3;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(120);
const MAX_ERROR_BODY_CHARS: usize = 4_096;

/// Async GitHub REST API client.
pub struct GitHubClient {
    client: Client,
    #[allow(dead_code)]
    token: String,
    /// Base URL for API requests (default: https://api.github.com).
    base_url: String,
    rate_limit_buffer: u32,
    /// Tracked remaining rate limit from response headers.
    rate_remaining: AtomicU32,
    /// Rate limit reset timestamp (Unix epoch seconds).
    rate_reset_at: AtomicU32,
}

/// Snapshot of current GitHub API rate limit status.
#[derive(Debug, Clone)]
pub struct RateLimitStatus {
    pub remaining: u32,
    pub reset_at: u32,
    pub is_low: bool,
}

impl GitHubClient {
    /// Create a new GitHub client with the given token.
    pub fn new(token: &str, rate_limit_buffer: u32) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| ContribError::GitHub(format!("Invalid token: {}", e)))?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("ContribAI/5.6"));

        let client = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ContribError::GitHub(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self {
            client,
            token: token.to_string(),
            base_url: GITHUB_API.to_string(),
            rate_limit_buffer,
            rate_remaining: AtomicU32::new(5000),
            rate_reset_at: AtomicU32::new(0),
        })
    }

    /// Override the base URL (for testing with mock servers).
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    /// Get current rate limit status.
    pub fn get_rate_status(&self) -> RateLimitStatus {
        let remaining = self.rate_remaining.load(Ordering::Relaxed);
        let reset_at = self.rate_reset_at.load(Ordering::Relaxed);
        RateLimitStatus {
            remaining,
            reset_at,
            is_low: remaining < self.rate_limit_buffer,
        }
    }

    /// Update tracked rate limit from response headers.
    fn track_rate_limit(&self, response: &Response) {
        if let Some(remaining) = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.rate_remaining.store(remaining, Ordering::Relaxed);
        }
        if let Some(reset) = response
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.rate_reset_at.store(reset, Ordering::Relaxed);
        }
    }

    /// Sleep if rate limit is critically low (< buffer threshold).
    async fn maybe_throttle(&self) {
        let remaining = self.rate_remaining.load(Ordering::Relaxed);
        if remaining < self.rate_limit_buffer {
            let reset = self.rate_reset_at.load(Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;
            let wait = reset.saturating_sub(now).min(120);
            if wait == 0 {
                return;
            }
            warn!(
                remaining,
                reset_in_secs = wait,
                "⏳ GitHub rate limit low, throttling"
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait as u64)).await;
        }
    }

    /// Only retry methods whose repeated execution has idempotent HTTP semantics.
    /// Retrying a POST/PATCH after an ambiguous network failure can create duplicate
    /// pull requests, comments, or issues.
    fn is_retry_safe(method: &reqwest::Method) -> bool {
        matches!(
            *method,
            reqwest::Method::GET
                | reqwest::Method::HEAD
                | reqwest::Method::PUT
                | reqwest::Method::DELETE
                | reqwest::Method::OPTIONS
        )
    }

    /// Prefer server-provided backoff, then GitHub's reset timestamp, and finally
    /// exponential backoff. The returned duration is intentionally not capped so
    /// callers can fail fast instead of parking an agent for an unexpectedly long time.
    fn retry_delay(headers: &HeaderMap, attempt: u32) -> Duration {
        if let Some(seconds) = headers
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            return Duration::from_secs(seconds);
        }

        if let Some(reset_at) = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            return Duration::from_secs(reset_at.saturating_sub(now));
        }

        Duration::from_secs(2u64.saturating_pow(attempt.saturating_sub(1)))
    }

    fn rate_limit_reset_description(headers: &HeaderMap) -> String {
        headers
            .get("x-ratelimit-reset")
            .or_else(|| headers.get(RETRY_AFTER))
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_string()
    }

    fn bounded_error_body(body: String) -> String {
        let mut chars = body.chars();
        let bounded: String = chars.by_ref().take(MAX_ERROR_BODY_CHARS).collect();
        if chars.next().is_some() {
            format!("{bounded}…")
        } else {
            bounded
        }
    }

    // ── Core HTTP ──────────────────────────────────────────────────────────

    /// Make an authenticated GitHub API request with error handling and retry.
    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        params: Option<&[(&str, &str)]>,
        json_body: Option<&Value>,
    ) -> Result<Value> {
        // v5.6: Proactive throttle before request
        self.maybe_throttle().await;

        let full_url = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{}", self.base_url, url)
        };

        let retry_safe = Self::is_retry_safe(&method);
        let mut last_error: Option<ContribError> = None;

        for attempt in 1..=MAX_REQUEST_ATTEMPTS {
            let mut req = self.client.request(method.clone(), &full_url);

            if let Some(p) = params {
                req = req.query(p);
            }
            if let Some(body) = json_body {
                req = req.json(body);
            }

            let response = match req.send().await {
                Ok(response) => response,
                Err(error) => {
                    let error = ContribError::GitHub(format!("HTTP error: {error}"));
                    if retry_safe && attempt < MAX_REQUEST_ATTEMPTS {
                        let delay = Self::retry_delay(&HeaderMap::new(), attempt);
                        warn!(
                            %url,
                            attempt,
                            max_attempts = MAX_REQUEST_ATTEMPTS,
                            delay_ms = delay.as_millis(),
                            error = %error,
                            "GitHub transport error, retrying"
                        );
                        last_error = Some(error);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error);
                }
            };

            let status = response.status();

            // v5.6: Track rate limit from every response
            self.track_rate_limit(&response);

            // Primary and secondary rate limits. GitHub can signal these as either
            // 403 or 429; secondary limits commonly include Retry-After.
            if status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::FORBIDDEN
                    && (response.headers().contains_key(RETRY_AFTER)
                        || response
                            .headers()
                            .get("x-ratelimit-remaining")
                            .is_some_and(|value| value == "0")))
            {
                let delay = Self::retry_delay(response.headers(), attempt);
                let reset_at = Self::rate_limit_reset_description(response.headers());
                if retry_safe && attempt < MAX_REQUEST_ATTEMPTS && delay <= MAX_RETRY_DELAY {
                    warn!(
                        %url,
                        attempt,
                        max_attempts = MAX_REQUEST_ATTEMPTS,
                        delay_ms = delay.as_millis(),
                        "GitHub rate limited request, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(ContribError::RateLimit { reset_at });
            }

            if status == StatusCode::FORBIDDEN {
                let body = Self::bounded_error_body(response.text().await.unwrap_or_default());
                return Err(ContribError::GitHub(format!("Forbidden: {}", body)));
            }

            if status == StatusCode::NOT_FOUND {
                return Err(ContribError::GitHub(format!("Not found: {}", url)));
            }

            // Retry on 5xx
            if status.is_server_error() {
                let delay = Self::retry_delay(response.headers(), attempt);
                let body = Self::bounded_error_body(response.text().await.unwrap_or_default());
                let error =
                    ContribError::GitHub(format!("GitHub API error {}: {}", status.as_u16(), body));
                if retry_safe && attempt < MAX_REQUEST_ATTEMPTS {
                    warn!(
                        status = status.as_u16(),
                        %url,
                        attempt,
                        max_attempts = MAX_REQUEST_ATTEMPTS,
                        delay_ms = delay.as_millis(),
                        "GitHub server error, retrying"
                    );
                    last_error = Some(error);
                    tokio::time::sleep(delay.min(MAX_RETRY_DELAY)).await;
                    continue;
                }
                return Err(error);
            }

            if status.is_client_error() {
                let body = Self::bounded_error_body(response.text().await.unwrap_or_default());
                return Err(ContribError::GitHub(format!(
                    "GitHub API error {}: {}",
                    status.as_u16(),
                    body
                )));
            }

            // Success — parse JSON
            let body = response.text().await.unwrap_or_default();
            if body.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&body)
                .map_err(|e| ContribError::GitHub(format!("JSON parse error: {}", e)));
        }

        Err(last_error.unwrap_or_else(|| ContribError::GitHub("Request attempts exhausted".into())))
    }

    /// Make a raw request returning the Response (for diffs, etc.).
    async fn request_raw(
        &self,
        method: reqwest::Method,
        url: &str,
        extra_headers: Option<HeaderMap>,
    ) -> Result<Response> {
        self.maybe_throttle().await;

        let full_url = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{}", self.base_url, url)
        };
        let retry_safe = Self::is_retry_safe(&method);
        let mut last_error: Option<ContribError> = None;

        for attempt in 1..=MAX_REQUEST_ATTEMPTS {
            let mut req = self.client.request(method.clone(), &full_url);
            if let Some(headers) = &extra_headers {
                req = req.headers(headers.clone());
            }

            let response = match req.send().await {
                Ok(response) => response,
                Err(error) => {
                    let error = ContribError::GitHub(format!("HTTP error: {error}"));
                    if retry_safe && attempt < MAX_REQUEST_ATTEMPTS {
                        let delay = Self::retry_delay(&HeaderMap::new(), attempt);
                        last_error = Some(error);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error);
                }
            };

            self.track_rate_limit(&response);
            if response.status().is_success() {
                return Ok(response);
            }

            let status = response.status();
            let delay = Self::retry_delay(response.headers(), attempt);
            let is_rate_limited = status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::FORBIDDEN
                    && (response.headers().contains_key(RETRY_AFTER)
                        || response
                            .headers()
                            .get("x-ratelimit-remaining")
                            .is_some_and(|value| value == "0")));

            if is_rate_limited {
                let reset_at = Self::rate_limit_reset_description(response.headers());
                if retry_safe && attempt < MAX_REQUEST_ATTEMPTS && delay <= MAX_RETRY_DELAY {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(ContribError::RateLimit { reset_at });
            }

            let body = Self::bounded_error_body(response.text().await.unwrap_or_default());
            let error =
                ContribError::GitHub(format!("GitHub API error {}: {}", status.as_u16(), body));
            if status.is_server_error() && retry_safe && attempt < MAX_REQUEST_ATTEMPTS {
                last_error = Some(error);
                tokio::time::sleep(delay.min(MAX_RETRY_DELAY)).await;
                continue;
            }
            return Err(error);
        }

        Err(last_error.unwrap_or_else(|| ContribError::GitHub("Request attempts exhausted".into())))
    }

    async fn get(&self, url: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, url, None, None).await
    }

    async fn get_with_params(&self, url: &str, params: &[(&str, &str)]) -> Result<Value> {
        self.request(reqwest::Method::GET, url, Some(params), None)
            .await
    }

    async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::POST, url, None, Some(body))
            .await
    }

    async fn put(&self, url: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PUT, url, None, Some(body))
            .await
    }

    async fn patch(&self, url: &str, body: &Value) -> Result<Value> {
        self.request(reqwest::Method::PATCH, url, None, Some(body))
            .await
    }

    async fn delete(&self, url: &str) -> Result<Value> {
        self.request(reqwest::Method::DELETE, url, None, None).await
    }

    // ── Rate Limit ─────────────────────────────────────────────────────────

    /// Check current rate limit status.
    pub async fn check_rate_limit(&self) -> Result<RateLimitInfo> {
        let data = self.get("/rate_limit").await?;
        let core = &data["resources"]["core"];
        let info = RateLimitInfo {
            remaining: core["remaining"].as_u64().unwrap_or(0) as u32,
            limit: core["limit"].as_u64().unwrap_or(0) as u32,
            reset: core["reset"].as_u64().unwrap_or(0),
        };
        info!(
            remaining = info.remaining,
            limit = info.limit,
            "Rate limit status"
        );
        Ok(info)
    }

    /// Ensure we have enough API calls remaining.
    pub async fn ensure_rate_limit(&self) -> Result<()> {
        let info = self.check_rate_limit().await?;
        if info.remaining < self.rate_limit_buffer {
            return Err(ContribError::RateLimit {
                reset_at: info.reset.to_string(),
            });
        }
        Ok(())
    }

    // ── Repository Operations ──────────────────────────────────────────────

    /// Search GitHub repositories.
    pub async fn search_repositories(
        &self,
        query: &str,
        sort: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Vec<Repository>> {
        let pp = per_page.to_string();
        let pg = page.to_string();
        let data = self
            .get_with_params(
                "/search/repositories",
                &[
                    ("q", query),
                    ("sort", sort),
                    ("order", "desc"),
                    ("per_page", &pp),
                    ("page", &pg),
                ],
            )
            .await?;

        let items = data["items"].as_array().cloned().unwrap_or_default();
        Ok(items.into_iter().map(|item| parse_repo(&item)).collect())
    }

    /// Get detailed repository information.
    pub async fn get_repo_details(&self, owner: &str, repo: &str) -> Result<Repository> {
        let data = self.get(&format!("/repos/{}/{}", owner, repo)).await?;
        Ok(parse_repo(&data))
    }

    /// Get the full file tree of a repository.
    pub async fn get_file_tree(
        &self,
        owner: &str,
        repo: &str,
        branch: Option<&str>,
    ) -> Result<Vec<FileNode>> {
        let branch = match branch {
            Some(b) => b.to_string(),
            None => {
                let details = self.get_repo_details(owner, repo).await?;
                details.default_branch
            }
        };

        let data = self
            .get_with_params(
                &format!("/repos/{}/{}/git/trees/{}", owner, repo, branch),
                &[("recursive", "1")],
            )
            .await?;

        let tree = data["tree"].as_array().cloned().unwrap_or_default();
        Ok(tree
            .into_iter()
            .map(|item| FileNode {
                path: item["path"].as_str().unwrap_or("").to_string(),
                node_type: item["type"].as_str().unwrap_or("blob").to_string(),
                size: item["size"].as_i64().unwrap_or(0),
                sha: item["sha"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// Get the content of a file from the repository.
    pub async fn get_file_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: Option<&str>,
    ) -> Result<String> {
        let url = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        let data = if let Some(r) = ref_name {
            self.get_with_params(&url, &[("ref", r)]).await?
        } else {
            self.get(&url).await?
        };

        if data["encoding"].as_str() == Some("base64") {
            let content = data["content"].as_str().unwrap_or("");
            let clean = content.replace(['\n', '\r'], "");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&clean)
                .map_err(|e| ContribError::GitHub(format!("Base64 decode error: {}", e)))?;
            return String::from_utf8(decoded)
                .map_err(|e| ContribError::GitHub(format!("UTF-8 decode error: {}", e)));
        }

        Ok(data["content"].as_str().unwrap_or("").to_string())
    }

    /// Get file content and blob SHA (needed for updates).
    pub async fn get_file_content_with_sha(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: Option<&str>,
    ) -> Result<(String, String)> {
        let url = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        let data = if let Some(r) = ref_name {
            self.get_with_params(&url, &[("ref", r)]).await?
        } else {
            self.get(&url).await?
        };

        let content = if data["encoding"].as_str() == Some("base64") {
            let raw = data["content"].as_str().unwrap_or("");
            let clean = raw.replace(['\n', '\r'], "");
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&clean)
                .map_err(|e| ContribError::GitHub(format!("Base64 decode: {}", e)))?;
            String::from_utf8(decoded)
                .map_err(|e| ContribError::GitHub(format!("UTF-8 decode: {}", e)))?
        } else {
            data["content"].as_str().unwrap_or("").to_string()
        };

        let sha = data["sha"].as_str().unwrap_or("").to_string();
        Ok((content, sha))
    }

    /// Get open issues for a repository.
    pub async fn get_open_issues(
        &self,
        owner: &str,
        repo: &str,
        per_page: u32,
    ) -> Result<Vec<Issue>> {
        let pp = per_page.to_string();
        let data = self
            .get_with_params(
                &format!("/repos/{}/{}/issues", owner, repo),
                &[("state", "open"), ("per_page", &pp)],
            )
            .await?;

        let items = data.as_array().cloned().unwrap_or_default();
        Ok(items
            .into_iter()
            .filter(|item| item.get("pull_request").is_none())
            .map(|item| Issue {
                number: item["number"].as_i64().unwrap_or(0),
                title: item["title"].as_str().unwrap_or("").to_string(),
                body: item["body"].as_str().map(String::from),
                labels: item["labels"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                state: item["state"].as_str().unwrap_or("open").to_string(),
                created_at: None,
                html_url: item["html_url"].as_str().unwrap_or("").to_string(),
            })
            .collect())
    }

    /// Fetch one issue so admission can revalidate its current state and labels.
    pub async fn get_issue(&self, owner: &str, repo: &str, issue_number: i64) -> Result<Issue> {
        let item = self
            .get(&format!(
                "/repos/{}/{}/issues/{}",
                owner, repo, issue_number
            ))
            .await?;
        if item.get("pull_request").is_some() {
            return Err(ContribError::GitHub(format!(
                "#{issue_number} is a pull request, not an issue"
            )));
        }
        Ok(Issue {
            number: item["number"].as_i64().unwrap_or(issue_number),
            title: item["title"].as_str().unwrap_or("").to_string(),
            body: item["body"].as_str().map(String::from),
            labels: item["labels"]
                .as_array()
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|label| label["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            state: item["state"].as_str().unwrap_or("").to_string(),
            created_at: None,
            html_url: item["html_url"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Try to fetch CONTRIBUTING.md.
    pub async fn get_contributing_guide(&self, owner: &str, repo: &str) -> Result<Option<String>> {
        for path in &[
            "CONTRIBUTING.md",
            "contributing.md",
            ".github/CONTRIBUTING.md",
        ] {
            match self.get_file_content(owner, repo, path, None).await {
                Ok(content) => return Ok(Some(content)),
                Err(_) => continue,
            }
        }
        Ok(None)
    }

    // ── Fork & Branch ──────────────────────────────────────────────────────

    /// Fork a repository to the authenticated user's account.
    pub async fn fork_repository(&self, owner: &str, repo: &str) -> Result<Repository> {
        let data = self
            .post(
                &format!("/repos/{}/{}/forks", owner, repo),
                &Value::Object(serde_json::Map::new()),
            )
            .await?;
        info!(
            fork = data["full_name"].as_str().unwrap_or("?"),
            "Forked {}/{}", owner, repo
        );
        Ok(parse_repo(&data))
    }

    /// Create a new branch from the default or specified branch.
    pub async fn create_branch(
        &self,
        owner: &str,
        repo: &str,
        branch_name: &str,
        from_branch: Option<&str>,
    ) -> Result<Value> {
        let from = match from_branch {
            Some(b) => b.to_string(),
            None => {
                let details = self.get_repo_details(owner, repo).await?;
                details.default_branch
            }
        };

        // Get SHA of source branch
        let ref_data = self
            .get(&format!("/repos/{}/{}/git/ref/heads/{}", owner, repo, from))
            .await?;
        let sha = ref_data["object"]["sha"].as_str().unwrap_or("").to_string();

        self.create_branch_at_sha(owner, repo, branch_name, &sha)
            .await
    }

    /// Create a branch at an exact attested commit rather than a moving branch head.
    pub async fn create_branch_at_sha(
        &self,
        owner: &str,
        repo: &str,
        branch_name: &str,
        sha: &str,
    ) -> Result<Value> {
        if sha.trim().is_empty() {
            return Err(ContribError::GitHub(
                "cannot create branch at an empty commit SHA".to_string(),
            ));
        }

        let data = self
            .post(
                &format!("/repos/{}/{}/git/refs", owner, repo),
                &serde_json::json!({
                    "ref": format!("refs/heads/{}", branch_name),
                    "sha": sha,
                }),
            )
            .await?;

        info!(branch = branch_name, "Created branch on {}/{}", owner, repo);
        Ok(data)
    }

    // ── Commit & PR ────────────────────────────────────────────────────────

    /// Create or update a file in the repository.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_or_update_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        content: &str,
        message: &str,
        branch: &str,
        sha: Option<&str>,
        signoff: Option<&str>,
    ) -> Result<Value> {
        let mut msg = message.to_string();
        if let Some(signer) = signoff {
            if !msg.contains("Signed-off-by:") {
                msg = format!("{}\n\nSigned-off-by: {}", msg, signer);
            }
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let mut payload = serde_json::json!({
            "message": msg,
            "content": encoded,
            "branch": branch,
        });
        if let Some(s) = sha {
            payload["sha"] = Value::String(s.to_string());
        }

        self.put(
            &format!("/repos/{}/{}/contents/{}", owner, repo, path),
            &payload,
        )
        .await
    }

    /// Create a draft pull request. Publishing is always left to maintainers.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: Option<&str>,
    ) -> Result<Value> {
        let base_branch = match base {
            Some(b) => b.to_string(),
            None => {
                let details = self.get_repo_details(owner, repo).await?;
                details.default_branch
            }
        };

        let data = self
            .post(
                &format!("/repos/{}/{}/pulls", owner, repo),
                &serde_json::json!({
                    "title": title,
                    "body": body,
                    "head": head,
                    "base": base_branch,
                    "draft": true,
                }),
            )
            .await?;

        info!(
            pr = data["number"].as_i64().unwrap_or(0),
            "Created draft PR on {}/{}: {}", owner, repo, title
        );
        Ok(data)
    }

    /// Close a pull request with an optional comment.
    pub async fn close_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
        comment: Option<&str>,
    ) -> Result<()> {
        if let Some(c) = comment {
            self.post(
                &format!("/repos/{}/{}/issues/{}/comments", owner, repo, pr_number),
                &serde_json::json!({ "body": c }),
            )
            .await?;
        }
        self.patch(
            &format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number),
            &serde_json::json!({ "state": "closed" }),
        )
        .await?;
        info!(pr = pr_number, "Closed PR on {}/{}", owner, repo);
        Ok(())
    }

    /// Update a PR's title and/or body.
    pub async fn update_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<Value> {
        let mut payload = serde_json::Map::new();
        if let Some(t) = title {
            payload.insert("title".into(), Value::String(t.into()));
        }
        if let Some(b) = body {
            payload.insert("body".into(), Value::String(b.into()));
        }
        self.patch(
            &format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number),
            &Value::Object(payload),
        )
        .await
    }

    // ── PR Comments & Reviews ──────────────────────────────────────────────

    /// Get comments on a pull request (issue comments).
    pub async fn get_pr_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<Value>> {
        let data = self
            .get(&format!(
                "/repos/{}/{}/issues/{}/comments",
                owner, repo, pr_number
            ))
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Post a comment on a pull request.
    pub async fn create_pr_comment(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
        body: &str,
    ) -> Result<Value> {
        self.post(
            &format!("/repos/{}/{}/issues/{}/comments", owner, repo, pr_number),
            &serde_json::json!({ "body": body }),
        )
        .await
    }

    /// Get reviews on a PR.
    pub async fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<Value>> {
        let data = self
            .get(&format!(
                "/repos/{}/{}/pulls/{}/reviews",
                owner, repo, pr_number
            ))
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Get inline code-level review comments on a PR.
    pub async fn get_pr_review_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<Value>> {
        let data = self
            .get(&format!(
                "/repos/{}/{}/pulls/{}/comments",
                owner, repo, pr_number
            ))
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Reply to an inline review comment.
    pub async fn create_pr_review_comment_reply(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
        comment_id: i64,
        body: &str,
    ) -> Result<Value> {
        self.post(
            &format!(
                "/repos/{}/{}/pulls/{}/comments/{}/replies",
                owner, repo, pr_number, comment_id
            ),
            &serde_json::json!({ "body": body }),
        )
        .await
    }

    /// Dismiss a pull request review.
    ///
    /// PUT /repos/{owner}/{repo}/pulls/{pull_number}/reviews/{review_id}/dismissals
    pub async fn dismiss_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: i64,
        review_id: i64,
        message: &str,
    ) -> Result<Value> {
        self.put(
            &format!(
                "/repos/{}/{}/pulls/{}/reviews/{}/dismissals",
                owner, repo, pull_number, review_id
            ),
            &serde_json::json!({
                "message": message,
                "event": "DISMISS"
            }),
        )
        .await
    }

    /// Check if CLA has been signed by looking at commit statuses.
    ///
    /// Fetches the PR head SHA, then queries the combined commit status.
    /// Returns true if a CLA-related status context reports "success".
    pub async fn check_cla_signed(
        &self,
        owner: &str,
        repo: &str,
        pull_number: i64,
    ) -> Result<bool> {
        // Get head SHA from PR details
        let pr = self.get_pr_details(owner, repo, pull_number).await?;
        let sha = pr["head"]["sha"]
            .as_str()
            .ok_or_else(|| ContribError::GitHub("PR head SHA not found".into()))?
            .to_string();

        // Fetch combined commit status
        let status = self
            .get(&format!("/repos/{}/{}/commits/{}/status", owner, repo, sha))
            .await?;

        let statuses = status["statuses"].as_array().cloned().unwrap_or_default();

        // Look for CLA-related contexts (case-insensitive)
        let cla_success = statuses.iter().any(|s| {
            let context = s["context"].as_str().unwrap_or("").to_lowercase();
            let state = s["state"].as_str().unwrap_or("");
            (context.contains("cla") || context.contains("license/cla")) && state == "success"
        });

        Ok(cla_success)
    }

    /// Get branch details.
    ///
    /// GET /repos/{owner}/{repo}/branches/{branch}
    pub async fn get_branch_info(&self, owner: &str, repo: &str, branch: &str) -> Result<Value> {
        self.get(&format!("/repos/{}/{}/branches/{}", owner, repo, branch))
            .await
    }

    /// Get the diff of a pull request.
    pub async fn get_pr_diff(&self, owner: &str, repo: &str, pr_number: i64) -> Result<String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.v3.diff"),
        );

        let response = self
            .request_raw(
                reqwest::Method::GET,
                &format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number),
                Some(headers),
            )
            .await?;

        response
            .text()
            .await
            .map_err(|e| ContribError::GitHub(format!("Failed to read diff: {}", e)))
    }

    /// Get the authenticated user's profile.
    pub async fn get_authenticated_user(&self) -> Result<Value> {
        self.get("/user").await
    }

    // ── Issues ─────────────────────────────────────────────────────────────

    /// List pull requests on a repository.
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        per_page: u32,
    ) -> Result<Vec<Value>> {
        let pp = per_page.min(100).to_string();
        let data = self
            .get_with_params(
                &format!("/repos/{}/{}/pulls", owner, repo),
                &[
                    ("state", state),
                    ("per_page", &pp),
                    ("sort", "created"),
                    ("direction", "desc"),
                ],
            )
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Get combined CI status for a commit ref.
    pub async fn get_combined_status(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<CiStatus> {
        let checks = self
            .get_with_params(
                &format!("/repos/{}/{}/commits/{}/check-runs", owner, repo, ref_name),
                &[("per_page", "100")],
            )
            .await
            .unwrap_or(Value::Null);

        let runs = checks["check_runs"].as_array().cloned().unwrap_or_default();

        if runs.is_empty() {
            return Ok(CiStatus {
                state: "pending".into(),
                total: 0,
                failed: vec![],
                passed: vec![],
                in_progress: vec![],
            });
        }

        let mut failed = vec![];
        let mut passed = vec![];
        let mut in_progress = vec![];

        for run in &runs {
            let name = run["name"].as_str().unwrap_or("").to_string();
            match run["conclusion"].as_str() {
                Some("failure") => failed.push(name),
                Some("success") => passed.push(name),
                _ => {
                    if matches!(run["status"].as_str(), Some("queued" | "in_progress")) {
                        in_progress.push(name);
                    }
                }
            }
        }

        let state = if !in_progress.is_empty() {
            "pending"
        } else if !failed.is_empty() {
            "failure"
        } else {
            "success"
        };

        Ok(CiStatus {
            state: state.into(),
            total: runs.len(),
            failed,
            passed,
            in_progress,
        })
    }

    // ── Fork Management ────────────────────────────────────────────────────

    /// List all forks owned by the authenticated user.
    pub async fn list_user_forks(&self) -> Result<Vec<Value>> {
        let data = self
            .get_with_params("/user/repos", &[("type", "fork"), ("per_page", "100")])
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Delete a repository.
    pub async fn delete_repository(&self, owner: &str, repo: &str) -> Result<()> {
        self.delete(&format!("/repos/{}/{}", owner, repo)).await?;
        Ok(())
    }

    /// Search GitHub issues across repositories.
    pub async fn search_issues(
        &self,
        query: &str,
        sort: &str,
        per_page: u32,
    ) -> Result<Vec<Value>> {
        let pp = per_page.to_string();
        let data = self
            .get_with_params(
                "/search/issues",
                &[
                    ("q", query),
                    ("sort", sort),
                    ("order", "desc"),
                    ("per_page", &pp),
                ],
            )
            .await?;
        Ok(data["items"].as_array().cloned().unwrap_or_default())
    }

    // ── GraphQL API ────────────────────────────────────────────────────────────

    /// Execute a GraphQL query against the GitHub API v4.
    ///
    /// Python equivalent: `github/client.py:graphql_query()`
    pub async fn graphql_query(&self, query: &str, variables: serde_json::Value) -> Result<Value> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let data = self
            .request(reqwest::Method::POST, "/graphql", None, Some(&body))
            .await?;

        if let Some(errors) = data["errors"].as_array() {
            let msg = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ContribError::GitHub(format!("GraphQL errors: {}", msg)));
        }

        Ok(data["data"].clone())
    }

    /// Search for solvable issues using GraphQL (richer data than REST).
    ///
    /// Returns issues with label info, comment counts, and author info.
    /// Python equivalent: `github/client.py:search_issues_graphql()`
    pub async fn search_issues_graphql(&self, query_str: &str, limit: usize) -> Result<Vec<Value>> {
        let query = r#"
            query($q: String!, $limit: Int!) {
                search(query: $q, type: ISSUE, first: $limit) {
                    nodes {
                        ... on Issue {
                            number
                            title
                            url
                            body
                            state
                            createdAt
                            updatedAt
                            comments { totalCount }
                            labels(first: 10) {
                                nodes { name }
                            }
                            repository {
                                nameWithOwner
                                stargazerCount
                                primaryLanguage { name }
                            }
                            assignees(first: 3) {
                                nodes { login }
                            }
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "q": query_str,
            "limit": limit,
        });

        let data = self.graphql_query(query, variables).await?;
        let nodes = data["search"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        Ok(nodes.into_iter().filter(|n| !n.is_null()).collect())
    }

    /// Get the blob SHA of a file (needed for updates).
    pub async fn get_file_sha(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: Option<&str>,
    ) -> Result<String> {
        let url = format!("/repos/{}/{}/contents/{}", owner, repo, path);
        let data = if let Some(r) = ref_name {
            self.get_with_params(&url, &[("ref", r)]).await?
        } else {
            self.get(&url).await?
        };
        Ok(data["sha"].as_str().unwrap_or("").to_string())
    }

    /// Get PR details.
    pub async fn get_pr_details(&self, owner: &str, repo: &str, pr_number: i64) -> Result<Value> {
        self.get(&format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number))
            .await
    }

    /// Get issues assigned to a user.
    pub async fn get_assigned_issues(
        &self,
        owner: &str,
        repo: &str,
        assignee: &str,
    ) -> Result<Vec<Value>> {
        let data = self
            .get_with_params(
                &format!("/repos/{}/{}/issues", owner, repo),
                &[("assignee", assignee), ("state", "open")],
            )
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Create an issue.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<Value> {
        self.post(
            &format!("/repos/{}/{}/issues", owner, repo),
            &serde_json::json!({ "title": title, "body": body }),
        )
        .await
    }

    /// Create an issue with labels.
    ///
    /// If the label does not exist on the target repository the API returns 422.
    /// Callers should catch the error and fall back to `create_issue` without labels.
    pub async fn create_issue_with_labels(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> Result<Value> {
        let labels_json: Vec<Value> = labels
            .iter()
            .map(|l| Value::String(l.to_string()))
            .collect();
        self.post(
            &format!("/repos/{}/{}/issues", owner, repo),
            &serde_json::json!({ "title": title, "body": body, "labels": labels_json }),
        )
        .await
    }

    /// Close an issue.
    pub async fn close_issue(
        &self,
        owner: &str,
        repo: &str,
        issue_number: i64,
        comment: Option<&str>,
    ) -> Result<()> {
        if let Some(c) = comment {
            self.post(
                &format!("/repos/{}/{}/issues/{}/comments", owner, repo, issue_number),
                &serde_json::json!({ "body": c }),
            )
            .await?;
        }
        self.patch(
            &format!("/repos/{}/{}/issues/{}", owner, repo, issue_number),
            &serde_json::json!({ "state": "closed" }),
        )
        .await?;
        Ok(())
    }

    /// Get comments on an issue (uses the issues comments endpoint).
    ///
    /// GitHub's issues comments API is shared with PRs — calling it with
    /// an issue number returns that issue's comments.
    pub async fn get_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<Vec<Value>> {
        let data = self
            .get(&format!(
                "/repos/{}/{}/issues/{}/comments",
                owner, repo, issue_number
            ))
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// Get the timeline events for an issue.
    ///
    /// Useful for detecting cross-references from pull requests
    /// (`event == "cross-referenced"` with `source.type == "issue"` and
    /// `source.issue.pull_request` present).
    pub async fn get_issue_timeline(
        &self,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<Vec<Value>> {
        let data = self
            .get_with_params(
                &format!("/repos/{}/{}/issues/{}/timeline", owner, repo, issue_number),
                &[("per_page", "100")],
            )
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    /// List open issues for a repository with optional label and assignee filters.
    ///
    /// Pass `labels` as a comma-separated string (e.g. `"bug,enhancement"`).
    /// Pass `assignee = Some("none")` to restrict to unassigned issues.
    pub async fn list_issues(
        &self,
        owner: &str,
        repo: &str,
        labels: Option<&str>,
        assignee: Option<&str>,
        per_page: u32,
    ) -> Result<Vec<Value>> {
        let pp = per_page.min(100).to_string();
        let mut params: Vec<(&str, &str)> = vec![("state", "open"), ("per_page", &pp)];
        if let Some(l) = labels {
            params.push(("labels", l));
        }
        if let Some(a) = assignee {
            params.push(("assignee", a));
        }
        let data = self
            .get_with_params(&format!("/repos/{}/{}/issues", owner, repo), &params)
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }
}

// ── Helper Types & Functions ──────────────────────────────────────────────────

/// Rate limit information.
#[derive(Debug)]
pub struct RateLimitInfo {
    pub remaining: u32,
    pub limit: u32,
    pub reset: u64,
}

/// CI status information.
#[derive(Debug)]
pub struct CiStatus {
    pub state: String,
    pub total: usize,
    pub failed: Vec<String>,
    pub passed: Vec<String>,
    pub in_progress: Vec<String>,
}

/// Parse raw API response into Repository model.
pub fn parse_repo(data: &Value) -> Repository {
    let owner = &data["owner"];
    Repository {
        owner: owner["login"].as_str().unwrap_or("").to_string(),
        name: data["name"].as_str().unwrap_or("").to_string(),
        full_name: data["full_name"].as_str().unwrap_or("").to_string(),
        description: data["description"].as_str().map(String::from),
        language: data["language"].as_str().map(String::from),
        languages: HashMap::new(),
        stars: data["stargazers_count"].as_i64().unwrap_or(0),
        forks: data["forks_count"].as_i64().unwrap_or(0),
        open_issues: data["open_issues_count"].as_i64().unwrap_or(0),
        topics: data["topics"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        default_branch: data["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string(),
        html_url: data["html_url"].as_str().unwrap_or("").to_string(),
        clone_url: data["clone_url"].as_str().unwrap_or("").to_string(),
        has_contributing: false,
        has_license: !data["license"].is_null(),
        last_push_at: None,
        created_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Clone)]
    struct FailThenSucceed {
        calls: Arc<AtomicUsize>,
        failures: usize,
        failure: ResponseTemplate,
        success: ResponseTemplate,
    }

    impl Respond for FailThenSucceed {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if call < self.failures {
                self.failure.clone()
            } else {
                self.success.clone()
            }
        }
    }

    fn minimal_repo_json() -> Value {
        serde_json::json!({
            "owner": { "login": "octocat" },
            "name": "hello-world",
            "full_name": "octocat/hello-world"
        })
    }

    #[test]
    fn test_parse_repo() {
        let json = serde_json::json!({
            "owner": { "login": "tang-vu" },
            "name": "ContribAI",
            "full_name": "tang-vu/ContribAI",
            "description": "AI agent",
            "language": "Python",
            "stargazers_count": 184,
            "forks_count": 10,
            "open_issues_count": 5,
            "topics": ["ai", "github"],
            "default_branch": "main",
            "html_url": "https://github.com/tang-vu/ContribAI",
            "clone_url": "https://github.com/tang-vu/ContribAI.git",
            "license": { "spdx_id": "MIT" }
        });

        let repo = parse_repo(&json);
        assert_eq!(repo.owner, "tang-vu");
        assert_eq!(repo.name, "ContribAI");
        assert_eq!(repo.stars, 184);
        assert!(repo.has_license);
        assert_eq!(repo.topics, vec!["ai", "github"]);
    }

    #[test]
    fn test_parse_repo_minimal() {
        let json = serde_json::json!({
            "owner": { "login": "test" },
            "name": "repo",
            "full_name": "test/repo"
        });

        let repo = parse_repo(&json);
        assert_eq!(repo.owner, "test");
        assert_eq!(repo.stars, 0);
        assert!(!repo.has_license);
        assert_eq!(repo.default_branch, "main");
    }

    #[test]
    fn rate_status_uses_configured_buffer() {
        let client = GitHubClient::new("token", 5_001).expect("client should build");
        assert!(client.get_rate_status().is_low);

        let client = GitHubClient::new("token", 5_000).expect("client should build");
        assert!(!client.get_rate_status().is_low);
    }

    #[test]
    fn retry_safety_prevents_duplicate_post_side_effects() {
        assert!(GitHubClient::is_retry_safe(&reqwest::Method::GET));
        assert!(GitHubClient::is_retry_safe(&reqwest::Method::PUT));
        assert!(!GitHubClient::is_retry_safe(&reqwest::Method::POST));
        assert!(!GitHubClient::is_retry_safe(&reqwest::Method::PATCH));
    }

    #[test]
    fn error_bodies_are_bounded_without_splitting_unicode() {
        let body = "🌏".repeat(MAX_ERROR_BODY_CHARS + 10);
        let bounded = GitHubClient::bounded_error_body(body);
        assert_eq!(bounded.chars().count(), MAX_ERROR_BODY_CHARS + 1);
        assert!(bounded.ends_with('…'));
    }

    #[tokio::test]
    async fn get_retries_transient_server_errors() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/repos/octocat/hello-world"))
            .respond_with(FailThenSucceed {
                calls: calls.clone(),
                failures: 1,
                failure: ResponseTemplate::new(503).insert_header("retry-after", "0"),
                success: ResponseTemplate::new(200).set_body_json(minimal_repo_json()),
            })
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        let repo = client
            .get_repo_details("octocat", "hello-world")
            .await
            .expect("GET should recover from a transient 503");

        assert_eq!(repo.full_name, "octocat/hello-world");
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn get_honors_retry_after_on_rate_limit() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/repos/octocat/hello-world"))
            .respond_with(FailThenSucceed {
                calls: calls.clone(),
                failures: 1,
                failure: ResponseTemplate::new(429).insert_header("retry-after", "0"),
                success: ResponseTemplate::new(200).set_body_json(minimal_repo_json()),
            })
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        client
            .get_repo_details("octocat", "hello-world")
            .await
            .expect("GET should recover from a short rate limit");

        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn post_is_not_retried_after_server_error() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/repos/octocat/hello-world/issues"))
            .respond_with(FailThenSucceed {
                calls: calls.clone(),
                failures: usize::MAX,
                failure: ResponseTemplate::new(503).insert_header("retry-after", "0"),
                success: ResponseTemplate::new(200),
            })
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        let result = client
            .create_issue("octocat", "hello-world", "title", "body")
            .await;

        assert!(result.is_err());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn raw_get_retries_and_tracks_rate_limit() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/repos/octocat/hello-world/pulls/7"))
            .respond_with(FailThenSucceed {
                calls: calls.clone(),
                failures: 1,
                failure: ResponseTemplate::new(503).insert_header("retry-after", "0"),
                success: ResponseTemplate::new(200)
                    .insert_header("x-ratelimit-remaining", "42")
                    .set_body_string("diff --git a/a.rs b/a.rs"),
            })
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        let diff = client
            .get_pr_diff("octocat", "hello-world", 7)
            .await
            .expect("raw GET should recover from a transient 503");

        assert!(diff.starts_with("diff --git"));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(client.get_rate_status().remaining, 42);
    }

    #[tokio::test]
    async fn graphql_uses_configured_endpoint_and_shared_rate_tracking() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-ratelimit-remaining", "17")
                    .set_body_json(serde_json::json!({
                        "data": { "viewer": { "login": "octocat" } }
                    })),
            )
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        let data = client
            .graphql_query("query { viewer { login } }", serde_json::json!({}))
            .await
            .expect("GraphQL query should use the configured endpoint");

        assert_eq!(data["viewer"]["login"], "octocat");
        assert_eq!(client.get_rate_status().remaining, 17);
    }

    #[tokio::test]
    async fn admitted_branches_are_bound_to_the_attested_sha() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/octocat/hello-world/git/refs"))
            .and(body_json(serde_json::json!({
                "ref": "refs/heads/contribai/fix",
                "sha": "attested123"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        client
            .create_branch_at_sha("octocat", "hello-world", "contribai/fix", "attested123")
            .await
            .expect("exact-SHA branch should be created");
    }

    #[tokio::test]
    async fn pull_requests_are_always_created_as_drafts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/octocat/hello-world/pulls"))
            .and(body_json(serde_json::json!({
                "title": "fix: parser",
                "body": "evidence",
                "head": "agent:contribai/fix",
                "base": "main",
                "draft": true
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "number": 7
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = GitHubClient::new("token", 100)
            .expect("client should build")
            .with_base_url(&server.uri());
        client
            .create_pull_request(
                "octocat",
                "hello-world",
                "fix: parser",
                "evidence",
                "agent:contribai/fix",
                Some("main"),
            )
            .await
            .expect("draft PR should be created");
    }
}
