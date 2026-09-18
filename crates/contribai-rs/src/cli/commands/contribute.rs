//! `contribai contribute` — drive a single authorized issue through the
//! full Contribution Run lifecycle.
//!
//! Wires the deterministic [`RunExecutor`] to production dependencies:
//! `GitHubClient` for repository reads, `LlmProvider` for the solver and
//! challenger models, `HumanReviewer` for the interactive gate, and
//! `PrManager::create_pr_with_evidence_v3` for the write path.

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::Utc;

use contribai::core::admission::{is_full_commit_sha, ContributionPermit, CONSENT_PATHS};
use contribai::core::challenge::{ChallengeCategory, ChallengeFinding, ChallengeSeverity};
use contribai::core::error::{ContribError, Result as CoreResult};
use contribai::core::evidence_v3::EvidenceCapsuleV3;
use contribai::core::models::{
    Contribution, ContributionType, FileChange, Finding, PrResult, Repository, Severity,
};
use contribai::core::run::{ContributionRun, RunState};
use contribai::core::task_spec::TaskSpec;
use contribai::generator::engine::ContributionGenerator;
use contribai::github::client::GitHubClient;
use contribai::llm::provider::LlmProvider;
use contribai::orchestrator::review_gate::{HumanReviewer, RunReviewDecision};
use contribai::orchestrator::run_executor::{
    OwnedTaskInputs, RunArtifacts, RunEnvironment, RunExecConfig, RunExecutor, RunIssueView,
    SolverCandidate,
};
use contribai::pr::manager::PrManager;

use crate::cli::common::{create_github, create_llm, create_memory, load_config, parse_github_url};

/// Maximum bytes fetched per file.
const SNAPSHOT_FILE_BYTES: usize = 64 * 1024;
/// Extensions skipped in the snapshot (binary/non-reviewable).
const BINARY_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".ico", ".pdf", ".zip", ".gz", ".tar", ".woff", ".woff2",
    ".ttf", ".eot", ".mp4", ".mp3", ".wasm", ".so", ".dll", ".exe", ".dylib", ".jar", ".class",
    ".o", ".a", ".lockb",
];

/// Production environment: GitHub reads + LLM solver/challenger + human
/// gate + v3 write path.
struct LiveRunEnv<'a> {
    github: &'a GitHubClient,
    llm: &'a dyn LlmProvider,
    challenger_llm: &'a dyn LlmProvider,
    reviewer: &'a HumanReviewer,
    solver_label: String,
    challenger_label: String,
    snapshot_file_limit: usize,
}

impl LiveRunEnv<'_> {
    fn split(repo: &str) -> CoreResult<(&str, &str)> {
        let mut parts = repo.splitn(2, '/');
        match (parts.next(), parts.next()) {
            (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => Ok((owner, name)),
            _ => Err(ContribError::Config(format!(
                "malformed repository {repo:?}"
            ))),
        }
    }
}

#[async_trait]
impl RunEnvironment for LiveRunEnv<'_> {
    async fn fetch_consent_manifest(&self, repo: &str, path: &str) -> CoreResult<Option<String>> {
        let (owner, name) = Self::split(repo)?;
        match self.github.get_file_content(owner, name, path, None).await {
            Ok(content) => Ok(Some(content)),
            Err(_) => Ok(None),
        }
    }

    async fn fetch_issue(&self, repo: &str, issue: i64) -> CoreResult<RunIssueView> {
        let (owner, name) = Self::split(repo)?;
        let issue_view = self.github.get_issue(owner, name, issue).await?;
        // Maintainer comments: best-effort; missing comments never block.
        let comments = self
            .github
            .get_issue_comments(owner, name, issue)
            .await
            .unwrap_or_default();
        let maintainer_comments = comments
            .iter()
            .filter(|c| {
                let assoc = c["author_association"].as_str().unwrap_or("");
                matches!(assoc, "OWNER" | "MEMBER" | "COLLABORATOR")
            })
            .filter_map(|c| c["body"].as_str().map(str::to_string))
            .collect();
        Ok(RunIssueView {
            number: issue_view.number,
            title: issue_view.title,
            body: issue_view.body.unwrap_or_default(),
            state: issue_view.state,
            labels: issue_view.labels,
            maintainer_comments,
        })
    }

    async fn fetch_repository(&self, repo: &str) -> CoreResult<Repository> {
        let (owner, name) = Self::split(repo)?;
        self.github.get_repo_details(owner, name).await
    }

    async fn attest_base_sha(&self, repo: &str, _issue: Option<i64>) -> CoreResult<String> {
        let (owner, name) = Self::split(repo)?;
        let details = self.github.get_repo_details(owner, name).await?;
        let branch = self
            .github
            .get_branch_info(owner, name, &details.default_branch)
            .await?;
        let sha = branch["commit"]["sha"].as_str().unwrap_or("").to_string();
        if sha.is_empty() {
            return Err(ContribError::Config(
                "default branch head could not be attested".into(),
            ));
        }
        Ok(sha)
    }

    async fn fetch_base_snapshot(
        &self,
        repo: &str,
        base_sha: &str,
    ) -> CoreResult<Vec<(String, Vec<u8>)>> {
        let (owner, name) = Self::split(repo)?;
        let tree = self
            .github
            .get_file_tree(owner, name, Some(base_sha))
            .await?;
        let mut files = Vec::new();
        for node in tree {
            if files.len() >= self.snapshot_file_limit {
                break;
            }
            if node.node_type != "blob" {
                continue;
            }
            if node.size > SNAPSHOT_FILE_BYTES as i64 {
                continue;
            }
            let lower = node.path.to_ascii_lowercase();
            if BINARY_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
                continue;
            }
            match self
                .github
                .get_file_content(owner, name, &node.path, Some(base_sha))
                .await
            {
                Ok(content) => files.push((node.path, content.into_bytes())),
                Err(_) => continue,
            }
        }
        Ok(files)
    }

    async fn task_inputs(&self, repo: &str, issue: Option<i64>) -> CoreResult<OwnedTaskInputs> {
        let (owner, name) = Self::split(repo)?;
        let mut inputs = OwnedTaskInputs::default();
        if let Some(number) = issue {
            let view = self.fetch_issue(repo, number).await?;
            inputs.issue_title = view.title;
            inputs.issue_body = view.body;
            inputs.maintainer_comments = view.maintainer_comments;
        }
        // Policy excerpt: the consent manifest itself is the authoritative
        // maintainer-authored scope text.
        for &path in CONSENT_PATHS {
            if let Ok(content) = self.github.get_file_content(owner, name, path, None).await {
                inputs.policy_excerpt = contribai::core::safe_truncate(&content, 2000).to_string();
                break;
            }
        }
        Ok(inputs)
    }

    async fn solve(
        &self,
        repo: &str,
        spec: &TaskSpec,
        workspace_paths: &[String],
    ) -> CoreResult<SolverCandidate> {
        let requirements = spec
            .requirements
            .iter()
            .map(|r| format!("- [{}] {}", r.provenance.as_str(), r.text))
            .collect::<Vec<_>>()
            .join("\n");
        let files = workspace_paths
            .iter()
            .take(60)
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You are solving one bounded maintainer-authorized task in {repo}.\n\n\
             Task: {title}\n\nRequirements:\n{requirements}\n\n\
             Repository files present:\n{files}\n\n\
             Return ONLY JSON of the form:\n\
             {{\"title\": \"...\", \"description\": \"...\", \"commit_message\": \"...\", \
               \"changes\": [{{\"path\": \"...\", \"new_content\": \"...\"}}]}}\n\n\
             Rules: minimum viable change; no new dependencies; no governance, \
             workflow, license, or security-policy files; no deletions.",
            title = spec.title,
        );
        let system = "You produce minimal, reviewable patches as strict JSON.";
        let response = self
            .llm
            .complete(&prompt, Some(system), Some(0.2), Some(8192))
            .await
            .map_err(|e| ContribError::Config(format!("solver: {e}")))?;
        parse_solver_output(&response, spec)
    }

    async fn repair(
        &self,
        repo: &str,
        spec: &TaskSpec,
        findings: &[ChallengeFinding],
        workspace_paths: &[String],
    ) -> CoreResult<Vec<FileChange>> {
        let findings_text = findings
            .iter()
            .filter(|f| !f.resolved)
            .map(|f| format!("- [{}] {}", f.severity.as_str(), f.summary))
            .collect::<Vec<_>>()
            .join("\n");
        let files = workspace_paths
            .iter()
            .take(60)
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Repair the candidate for task {:?} in {repo}.\n\n\
             Challenger findings to resolve:\n{findings_text}\n\n\
             Repository files present:\n{files}\n\n\
             Return ONLY JSON: {{\"changes\": [{{\"path\": \"...\", \"new_content\": \"...\"}}]}}\n\
             Minimal fix addressing the findings; same scope rules as before.",
            spec.title,
        );
        let response = self
            .llm
            .complete(
                &prompt,
                Some("Return strict JSON only."),
                Some(0.2),
                Some(8192),
            )
            .await
            .map_err(|e| ContribError::Config(format!("repair: {e}")))?;
        Ok(parse_changes_json(&response))
    }

    async fn challenge(
        &self,
        repo: &str,
        spec: &TaskSpec,
        diff_summary: &str,
    ) -> CoreResult<Vec<ChallengeFinding>> {
        let requirements = spec
            .requirements
            .iter()
            .map(|r| format!("- {}", r.text))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You are an adversarial reviewer. Attack this candidate change in {repo}.\n\n\
             Task requirements:\n{requirements}\n\n\
             Changed files (path +lines/-lines):\n{diff_summary}\n\n\
             Look for: misunderstood requirements, regressions, missing edge cases, \
             weak tests, security defects, scope expansion, hidden dependency changes, \
             brittleness, generated-code artifacts.\n\
             Return ONLY JSON: {{\"findings\": [{{\"severity\": \"critical|high|medium|low|info\", \
             \"category\": \"misunderstood_requirement|regression|missing_edge_case|weak_tests|\
             security|compatibility|scope_expansion|dependency_change|brittleness|\
             generated_artifact|governance_violation|other\", \
             \"summary\": \"one paragraph\", \"file_path\": \"optional\"}}]}}\n\
             Empty findings array if the candidate is sound."
        );
        let response = self
            .challenger_llm
            .complete(
                &prompt,
                Some("You are a strict adversarial reviewer. JSON only."),
                Some(0.2),
                Some(4096),
            )
            .await
            .map_err(|e| ContribError::Config(format!("challenger: {e}")))?;
        Ok(parse_findings_json(&response))
    }

    async fn human_review(
        &self,
        contribution: &Contribution,
        repo_name: &str,
        evidence: &EvidenceCapsuleV3,
    ) -> CoreResult<RunReviewDecision> {
        self.reviewer
            .review_run_candidate(contribution, &contribution.finding, repo_name, evidence)
            .await
    }

    async fn submit(
        &self,
        contribution: &Contribution,
        repo: &Repository,
        evidence: &EvidenceCapsuleV3,
        run: &ContributionRun,
        permit: &ContributionPermit,
    ) -> CoreResult<PrResult> {
        let mut manager = PrManager::new(self.github);
        manager
            .create_pr_with_evidence_v3(contribution, repo, evidence, run, permit)
            .await
    }

    fn solver_model_label(&self) -> Option<String> {
        Some(self.solver_label.clone())
    }
    fn challenger_model_label(&self) -> Option<String> {
        Some(self.challenger_label.clone())
    }
}

/// Parse solver JSON into a `SolverCandidate`, defensively.
fn parse_solver_output(response: &str, spec: &TaskSpec) -> CoreResult<SolverCandidate> {
    let json_text = ContributionGenerator::extract_json(response)
        .ok_or_else(|| ContribError::Config("solver returned no parseable JSON".into()))?;
    let data: serde_json::Value = serde_json::from_str(&json_text)
        .map_err(|e| ContribError::Config(format!("solver JSON: {e}")))?;

    let changes = parse_changes_json(response);
    if changes.is_empty() {
        return Err(ContribError::Config(
            "solver returned no file changes".into(),
        ));
    }

    let title = data["title"].as_str().unwrap_or(&spec.title).to_string();
    let description = data["description"].as_str().unwrap_or("").to_string();
    let commit_message = data["commit_message"]
        .as_str()
        .unwrap_or(&title)
        .to_string();
    let first_path = changes.first().map(|c| c.path.clone()).unwrap_or_default();

    Ok(SolverCandidate {
        title,
        description,
        commit_message,
        contribution_type: ContributionType::CodeQuality,
        finding: Finding {
            id: String::new(),
            finding_type: ContributionType::CodeQuality,
            severity: Severity::Medium,
            title: spec.title.clone(),
            description: spec
                .requirements
                .first()
                .map(|r| r.text.clone())
                .unwrap_or_default(),
            file_path: first_path,
            line_start: None,
            line_end: None,
            suggestion: None,
            confidence: 0.8,
            priority_signals: vec![],
        },
        changes,
        tests_added: Vec::new(),
    })
}

/// Extract `changes`/`tests` file lists from solver/repair JSON.
fn parse_changes_json(response: &str) -> Vec<FileChange> {
    let Some(json_text) = ContributionGenerator::extract_json(response) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["changes", "tests_added", "tests"] {
        if let Some(items) = data.get(key).and_then(|v| v.as_array()) {
            for item in items {
                let Some(path) = item["path"].as_str() else {
                    continue;
                };
                let new_content = item["new_content"]
                    .as_str()
                    .or_else(|| item["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                if path.trim().is_empty() || new_content.is_empty() {
                    continue;
                }
                out.push(FileChange {
                    path: path.to_string(),
                    original_content: None,
                    new_content,
                    is_new_file: item["is_new_file"].as_bool().unwrap_or(false),
                    is_deleted: false,
                });
            }
        }
    }
    // Bare-array fallback: [{path, new_content}]
    if out.is_empty() {
        if let Some(items) = data.as_array() {
            for item in items {
                if let (Some(path), Some(content)) =
                    (item["path"].as_str(), item["new_content"].as_str())
                {
                    out.push(FileChange {
                        path: path.to_string(),
                        original_content: None,
                        new_content: content.to_string(),
                        is_new_file: item["is_new_file"].as_bool().unwrap_or(false),
                        is_deleted: false,
                    });
                }
            }
        }
    }
    out
}

/// Parse challenger findings JSON, defensively.
fn parse_findings_json(response: &str) -> Vec<ChallengeFinding> {
    let Some(json_text) = ContributionGenerator::extract_json(response) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return Vec::new();
    };
    let items = data
        .get("findings")
        .and_then(|v| v.as_array())
        .or_else(|| data.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .take(50)
        .filter_map(|item| {
            let severity = ChallengeSeverity::parse(item["severity"].as_str().unwrap_or("info"));
            let category = ChallengeCategory::parse(item["category"].as_str().unwrap_or("other"));
            let summary = item["summary"].as_str().unwrap_or("").to_string();
            if summary.is_empty() {
                return None;
            }
            Some(ChallengeFinding {
                severity,
                category,
                summary: contribai::core::safe_truncate(&summary, 500).to_string(),
                file_path: item["file_path"].as_str().map(str::to_string),
                resolved: false,
            })
        })
        .collect()
}

/// `contribai contribute <repo> --issue <N>` — the v7 lifecycle entrypoint.
pub async fn run_contribute(
    config_path: Option<&str>,
    url: &str,
    issue: i64,
    dry_run: bool,
    submit: bool,
    json: bool,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let github = create_github(&config)?;
    let llm = create_llm(&config)?;
    let memory = create_memory(&config)?;
    let (owner, name) = parse_github_url(url)?;
    let repo_name = format!("{owner}/{name}");

    // Attest the base revision BEFORE creating the run — a run never floats.
    let details = github.get_repo_details(&owner, &name).await?;
    let branch = github
        .get_branch_info(&owner, &name, &details.default_branch)
        .await?;
    let base_sha = branch["commit"]["sha"].as_str().unwrap_or("").to_string();
    if !is_full_commit_sha(&base_sha) {
        anyhow::bail!("base revision could not be attested to a full commit SHA");
    }

    let runs_root = config.run.resolved_runs_root(&config.storage);

    let reviewer = HumanReviewer::interactive();
    let solver_label = config.llm.provider.clone();
    let env = LiveRunEnv {
        github: &github,
        llm: llm.as_ref(),
        challenger_llm: llm.as_ref(),
        reviewer: &reviewer,
        solver_label: solver_label.clone(),
        challenger_label: solver_label,
        snapshot_file_limit: config.run.snapshot_file_limit,
    };

    let exec_config = RunExecConfig {
        runs_root: runs_root.clone(),
        max_repair_iterations: config.run.max_repair_iterations,
        allow_approval_commands: config.run.allow_approval_commands,
        command_timeout_secs: config.run.command_timeout_secs,
        submit_capable: submit && !dry_run,
    };
    let executor = RunExecutor::new(&memory, &env, exec_config);

    // Create the run bound to the attested SHA.
    let run = ContributionRun::new(
        repo_name.clone(),
        Some(issue),
        base_sha.clone(),
        Utc::now() + chrono::Duration::seconds(config.run.run_ttl_seconds as i64),
    );
    memory.insert_run(&run)?;
    if !json {
        println!(
            "run {} created for {repo_name}#{issue} @ {base_sha}",
            run.run_id
        );
    }

    let final_run = if dry_run {
        executor.drive_to_evidence(&run.run_id).await
    } else {
        executor.drive(&run.run_id).await
    };

    match final_run {
        Ok(run) => {
            if json {
                let artifacts = RunArtifacts::load(&runs_root, &run.run_id).ok();
                print!("{}", run_report_json(&run, artifacts.as_ref()));
            } else {
                print_run_summary(&run);
            }
            Ok(())
        }
        Err(error) => {
            let run = memory.get_run(&run.run_id)?.unwrap_or(run);
            if json {
                print!("{}", run_report_json(&run, None));
            } else {
                eprintln!("run {} stopped: {error}", run.run_id);
                print_run_summary(&run);
            }
            Err(anyhow!(
                "run {} ended in {}",
                run.run_id,
                run.state.as_str()
            ))
        }
    }
}

/// `contribai runs` — list contribution runs.
pub fn run_runs(
    config_path: Option<&str>,
    repository: Option<&str>,
    state: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let memory = create_memory(&config)?;
    let state = state
        .map(|s| RunState::parse(s).ok_or_else(|| anyhow!("unknown run state {s:?}")))
        .transpose()?;
    let runs = memory.list_runs(repository, state, limit)?;
    if json {
        let payload: Vec<serde_json::Value> = runs.iter().map(run_summary_json).collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        if runs.is_empty() {
            println!("no contribution runs found");
            return Ok(());
        }
        println!(
            "{:<30} {:<14} {:<24} issue",
            "run_id", "state", "repository"
        );
        for run in &runs {
            println!(
                "{:<30} {:<14} {:<24} {}",
                run.run_id,
                run.state.as_str(),
                run.repository,
                run.issue.map(|i| i.to_string()).unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// `contribai inspect-run <run_id>` — full lifecycle inspectability.
pub fn run_inspect_run(config_path: Option<&str>, run_id: &str, json: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let memory = create_memory(&config)?;
    let run = memory
        .get_run(run_id)?
        .ok_or_else(|| anyhow!("run {run_id} does not exist"))?;
    let events = memory.get_run_events(run_id)?;
    let runs_root = config.run.resolved_runs_root(&config.storage);
    let artifacts = RunArtifacts::load(&runs_root, run_id).ok();

    if json {
        let mut payload = run_summary_json(&run);
        payload["events"] = serde_json::to_value(
            events
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "from": e.state_from.as_str(),
                        "to": e.state_to.as_str(),
                        "event": e.event,
                        "detail": e.detail,
                        "recorded_at": e.recorded_at.to_rfc3339(),
                    })
                })
                .collect::<Vec<_>>(),
        )?;
        if let Some(artifacts) = &artifacts {
            payload["task_spec"] = serde_json::to_value(&artifacts.task_spec)?;
            payload["reproduction"] = serde_json::to_value(&artifacts.reproduction)?;
            payload["challenge"] = serde_json::to_value(&artifacts.challenge)?;
            payload["review_surface"] = serde_json::to_value(&artifacts.review_surface)?;
            payload["capsule"] = serde_json::to_value(&artifacts.capsule)?;
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("run           : {}", run.run_id);
    println!("repository    : {}", run.repository);
    println!("issue         : {:?}", run.issue);
    println!("state         : {}", run.state.as_str());
    println!("base          : {}", run.base_sha);
    if let Some(p) = &run.permit_id {
        println!("permit        : {p}");
    }
    if let Some(fp) = &run.task_fingerprint {
        println!("task fp       : {fp}");
    }
    if let Some(fp) = &run.candidate_fingerprint {
        println!("candidate fp  : {fp}");
    }
    if let Some(fp) = &run.review_fingerprint {
        println!("review fp     : {fp}");
    }
    if let Some(repro) = &run.reproduction {
        println!("reproduction  : {repro}");
    }
    if let Some(summary) = &run.challenge_summary {
        println!("challenge     : {summary}");
    }
    if run.repair_iterations > 0 {
        println!("repairs       : {}", run.repair_iterations);
    }
    if let Some(reason) = &run.terminal_reason {
        println!("terminal      : {reason}");
    }
    if let Some(url) = &run.draft_pr_url {
        println!("draft PR      : {url}");
    }
    if let Some(surface) = artifacts.as_ref().and_then(|a| a.review_surface.as_ref()) {
        println!("\nreview surface:");
        for line in &surface.summary_lines {
            println!("  - {line}");
        }
    }
    if let Some(capsule) = artifacts.as_ref().and_then(|a| a.capsule.as_ref()) {
        println!("\nchecks:");
        for check in &capsule.checks {
            println!(
                "  [{}] {}: {}",
                if check.passed { "pass" } else { "FAIL" },
                check.name,
                check.details
            );
        }
    }
    if !events.is_empty() {
        println!("\nlifecycle:");
        for e in &events {
            println!(
                "  {} {} → {} ({})",
                e.recorded_at.format("%H:%M:%S"),
                e.state_from.as_str(),
                e.state_to.as_str(),
                e.event
            );
        }
    }
    Ok(())
}

fn run_summary_json(run: &ContributionRun) -> serde_json::Value {
    serde_json::json!({
        "run_id": run.run_id,
        "repository": run.repository,
        "issue": run.issue,
        "state": run.state.as_str(),
        "base_sha": run.base_sha,
        "permit_id": run.permit_id,
        "task_fingerprint": run.task_fingerprint,
        "candidate_fingerprint": run.candidate_fingerprint,
        "review_fingerprint": run.review_fingerprint,
        "reproduction": run.reproduction,
        "challenge_summary": run.challenge_summary,
        "repair_iterations": run.repair_iterations,
        "draft_pr_number": run.draft_pr_number,
        "draft_pr_url": run.draft_pr_url,
        "terminal_reason": run.terminal_reason,
        "solver_model": run.solver_model,
        "challenger_model": run.challenger_model,
        "created_at": run.created_at.to_rfc3339(),
        "updated_at": run.updated_at.to_rfc3339(),
        "expires_at": run.expires_at.to_rfc3339(),
    })
}

fn run_report_json(run: &ContributionRun, artifacts: Option<&RunArtifacts>) -> String {
    let mut payload = run_summary_json(run);
    if let Some(artifacts) = artifacts {
        payload["capsule"] =
            serde_json::to_value(&artifacts.capsule).unwrap_or(serde_json::Value::Null);
        payload["review_surface"] =
            serde_json::to_value(&artifacts.review_surface).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

fn print_run_summary(run: &ContributionRun) {
    println!("\nrun {}", run.run_id);
    println!("  state     : {}", run.state.as_str());
    if let Some(reason) = &run.terminal_reason {
        println!("  terminal  : {reason}");
    }
    if let Some(url) = &run.draft_pr_url {
        println!("  draft PR  : {url}");
    }
    if let Some(fp) = &run.candidate_fingerprint {
        println!("  candidate : {fp}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solver_output_parses_canonical_json() {
        let spec = TaskSpec::draft("o/r", Some(1), "fix");
        let response = r#"{"title":"fix x","description":"d","commit_message":"fix: x",
            "changes":[{"path":"src/a.rs","new_content":"fn a() {}\n"}]}"#;
        let candidate = parse_solver_output(response, &spec).unwrap();
        assert_eq!(candidate.changes.len(), 1);
        assert_eq!(candidate.changes[0].path, "src/a.rs");
        assert_eq!(candidate.title, "fix x");
    }

    #[test]
    fn solver_output_rejects_empty_changes() {
        let spec = TaskSpec::draft("o/r", Some(1), "fix");
        assert!(parse_solver_output(r#"{"title":"x","changes":[]}"#, &spec).is_err());
        assert!(parse_solver_output("not json", &spec).is_err());
    }

    #[test]
    fn findings_parse_with_severity_and_category() {
        let response = r#"{"findings":[{"severity":"high","category":"missing_edge_case",
            "summary":"empty input panics","file_path":"src/a.rs"}]}"#;
        let findings = parse_findings_json(response);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, ChallengeSeverity::High);
        assert!(findings[0].severity.is_concern());
    }

    #[test]
    fn findings_tolerate_bare_array_and_junk() {
        assert!(parse_findings_json("garbage").is_empty());
        let arr = r#"[{"severity":"low","category":"other","summary":"nit"}]"#;
        assert_eq!(parse_findings_json(arr).len(), 1);
        // Missing summary → dropped.
        let bad = r#"{"findings":[{"severity":"high"}]}"#;
        assert!(parse_findings_json(bad).is_empty());
    }
}
