//! Maintainer-controlled admission for AI-mediated contributions.
//!
//! This module deliberately separates generating a change from earning permission
//! to publish it. A contribution is publishable only when it carries an explicit,
//! time-bounded permit rooted in repository or maintainer consent and passes the
//! scope policy. The resulting evidence capsule is deterministic and auditable.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::models::{Contribution, FileChange, Issue, Repository};
use crate::github::client::GitHubClient;

/// Repository files that explicitly enable ContribAI writes.
///
/// The YAML manifest is canonical. The marker-style paths remain readable for
/// compatibility with the experimental v1 protocol.
pub const CONSENT_PATHS: &[&str] = &[
    ".github/contribai.yml",
    ".github/CONTRIBAI_ALLOW",
    "CONTRIBAI_ALLOW",
];

/// Labels that only repository collaborators with triage permission can normally apply.
pub const MAINTAINER_APPROVAL_LABELS: &[&str] = &[
    "agent-ready",
    "contribai-approved",
    "ai-contribution-approved",
];

const DEFAULT_MAX_FILES: usize = 5;
const DEFAULT_MAX_CHANGED_LINES: usize = 250;
const DEFAULT_PERMIT_TTL_HOURS: i64 = 24;
const CONSENT_SCHEMA_VERSION: u8 = 1;
const EVIDENCE_SCHEMA_VERSION: u8 = 2;

/// Source of the maintainer's consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConsentSource {
    RepositoryManifest { path: String },
    MaintainerLabel { issue: i64, label: String },
}

/// Parsed repository-side consent and its review budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConsent {
    pub source: ConsentSource,
    pub max_files: usize,
    pub max_changed_lines: usize,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    pub draft_only: bool,
}

impl RepositoryConsent {
    /// Parse the intentionally small consent manifest.
    ///
    /// A manifest is valid only when it contains `enabled: true`. Unknown fields
    /// and unsupported schema versions fail closed so typos cannot widen scope.
    pub fn parse(path: &str, content: &str) -> Option<Self> {
        let manifest: ConsentManifest = serde_yaml::from_str(content).ok()?;
        if !manifest.enabled
            || manifest
                .schema_version
                .is_some_and(|version| version != CONSENT_SCHEMA_VERSION)
        {
            return None;
        }
        let max_files = manifest.max_files.unwrap_or(DEFAULT_MAX_FILES);
        let max_changed_lines = manifest
            .max_changed_lines
            .unwrap_or(DEFAULT_MAX_CHANGED_LINES);
        if max_files == 0 || max_changed_lines == 0 {
            return None;
        }

        let allowed_paths = manifest.allowed_paths.into_paths();
        if allowed_paths
            .iter()
            .any(|pattern| !is_safe_allow_pattern(pattern))
        {
            return None;
        }

        Some(Self {
            source: ConsentSource::RepositoryManifest {
                path: path.to_string(),
            },
            max_files,
            max_changed_lines,
            allowed_paths,
            draft_only: true,
        })
    }

    /// Construct consent from a maintainer-controlled issue label.
    pub fn from_issue(issue: &Issue) -> Option<Self> {
        if !issue.state.eq_ignore_ascii_case("open") {
            return None;
        }
        let label = issue.labels.iter().find(|label| {
            MAINTAINER_APPROVAL_LABELS
                .iter()
                .any(|allowed| label.eq_ignore_ascii_case(allowed))
        })?;
        Some(Self {
            source: ConsentSource::MaintainerLabel {
                issue: issue.number,
                label: label.clone(),
            },
            max_files: DEFAULT_MAX_FILES,
            max_changed_lines: DEFAULT_MAX_CHANGED_LINES,
            allowed_paths: Vec::new(),
            draft_only: true,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentManifest {
    schema_version: Option<u8>,
    #[serde(default)]
    enabled: bool,
    max_files: Option<usize>,
    max_changed_lines: Option<usize>,
    #[serde(default)]
    allowed_paths: AllowedPaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum AllowedPaths {
    List(Vec<String>),
    CommaSeparated(String),
    #[default]
    Empty,
}

impl AllowedPaths {
    fn into_paths(self) -> Vec<String> {
        let paths = match self {
            Self::List(paths) => paths,
            Self::CommaSeparated(paths) => paths.split(',').map(str::to_string).collect(),
            Self::Empty => Vec::new(),
        };
        paths
            .into_iter()
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty())
            .collect()
    }
}

/// Discover explicit repository-side consent. Missing, empty, or malformed files
/// are a denial, never an implicit approval.
pub async fn discover_repository_consent(
    github: &GitHubClient,
    owner: &str,
    repo: &str,
) -> Option<RepositoryConsent> {
    for path in CONSENT_PATHS {
        if let Ok(content) = github.get_file_content(owner, repo, path, None).await {
            if let Some(consent) = RepositoryConsent::parse(path, &content) {
                return Some(consent);
            }
        }
    }
    None
}

/// A time-bounded capability to propose exactly one contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPermit {
    pub id: String,
    pub repository: String,
    pub base_sha: String,
    pub source: ConsentSource,
    pub issue: Option<i64>,
    pub allowed_paths: Vec<String>,
    pub max_files: usize,
    pub max_changed_lines: usize,
    pub draft_only: bool,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl ContributionPermit {
    pub fn issue(
        repository: &Repository,
        base_sha: impl Into<String>,
        consent: RepositoryConsent,
        issue: Option<i64>,
    ) -> Self {
        let issued_at = Utc::now();
        let mut permit = Self {
            id: String::new(),
            repository: repository.full_name.clone(),
            base_sha: base_sha.into(),
            source: consent.source,
            issue,
            allowed_paths: consent.allowed_paths,
            max_files: consent.max_files,
            max_changed_lines: consent.max_changed_lines,
            draft_only: consent.draft_only,
            issued_at,
            expires_at: issued_at + Duration::hours(DEFAULT_PERMIT_TTL_HOURS),
        };
        permit.id = permit.fingerprint();
        permit
    }

    fn fingerprint(&self) -> String {
        let material = format!(
            "v2\n{}\n{}\n{:?}\n{:?}\n{:?}\n{}\n{}\n{}\n{}\n{}",
            self.repository,
            self.base_sha,
            self.source,
            self.issue,
            self.allowed_paths,
            self.max_files,
            self.max_changed_lines,
            self.draft_only,
            self.issued_at.timestamp(),
            self.expires_at.timestamp()
        );
        short_sha256(material.as_bytes())
    }
}

/// Stable reason why a contribution did not earn admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionViolation {
    ExternalWritesNotEnabled,
    MissingConsent,
    MissingBaseRevision,
    InvalidBaseRevision,
    PermitExpired,
    RepositoryMismatch,
    TooManyFiles { actual: usize, maximum: usize },
    TooManyChangedLines { actual: usize, maximum: usize },
    ProtectedPath { path: String },
    InvalidPath { path: String, reason: String },
    DuplicatePath { path: String },
    UnsupportedDeletion { path: String },
    PathOutsidePermit { path: String },
}

impl std::fmt::Display for AdmissionViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExternalWritesNotEnabled => write!(formatter, "external writes were not enabled"),
            Self::MissingConsent => write!(formatter, "maintainer consent was not found"),
            Self::MissingBaseRevision => write!(formatter, "base revision could not be attested"),
            Self::InvalidBaseRevision => write!(formatter, "base revision is not a full Git SHA"),
            Self::PermitExpired => write!(formatter, "contribution permit expired"),
            Self::RepositoryMismatch => write!(formatter, "permit belongs to another repository"),
            Self::TooManyFiles { actual, maximum } => {
                write!(formatter, "changed {actual} files; permit allows {maximum}")
            }
            Self::TooManyChangedLines { actual, maximum } => {
                write!(formatter, "changed {actual} lines; permit allows {maximum}")
            }
            Self::ProtectedPath { path } => write!(formatter, "protected path: {path}"),
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid repository path {path:?}: {reason}")
            }
            Self::DuplicatePath { path } => write!(formatter, "duplicate repository path: {path}"),
            Self::UnsupportedDeletion { path } => {
                write!(formatter, "file deletion is not supported: {path}")
            }
            Self::PathOutsidePermit { path } => write!(formatter, "path outside permit: {path}"),
        }
    }
}

/// Complete admission report. An empty violation list is the only allow state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionReport {
    pub allowed: bool,
    pub violations: Vec<AdmissionViolation>,
    pub file_count: usize,
    pub changed_lines: usize,
    pub paths: Vec<String>,
}

pub struct AdmissionController;

impl AdmissionController {
    pub fn evaluate(
        repository: &Repository,
        contribution: &Contribution,
        permit: &ContributionPermit,
        now: DateTime<Utc>,
    ) -> AdmissionReport {
        let changes: Vec<&FileChange> = contribution
            .changes
            .iter()
            .chain(contribution.tests_added.iter())
            .collect();
        let paths: Vec<String> = changes.iter().map(|change| change.path.clone()).collect();
        let changed_lines = changes
            .iter()
            .map(|change| changed_line_count(change))
            .sum();
        let mut violations = Vec::new();

        if permit.repository != repository.full_name {
            violations.push(AdmissionViolation::RepositoryMismatch);
        }
        if permit.base_sha.trim().is_empty() {
            violations.push(AdmissionViolation::MissingBaseRevision);
        } else if !is_full_commit_sha(&permit.base_sha) {
            violations.push(AdmissionViolation::InvalidBaseRevision);
        }
        if now > permit.expires_at {
            violations.push(AdmissionViolation::PermitExpired);
        }
        if paths.len() > permit.max_files {
            violations.push(AdmissionViolation::TooManyFiles {
                actual: paths.len(),
                maximum: permit.max_files,
            });
        }
        if changed_lines > permit.max_changed_lines {
            violations.push(AdmissionViolation::TooManyChangedLines {
                actual: changed_lines,
                maximum: permit.max_changed_lines,
            });
        }

        let mut unique_paths = BTreeSet::new();
        for change in changes {
            let path = &change.path;
            if let Some(reason) = repository_path_error(path) {
                violations.push(AdmissionViolation::InvalidPath {
                    path: path.clone(),
                    reason: reason.to_string(),
                });
                continue;
            }
            if !unique_paths.insert(path.clone()) {
                violations.push(AdmissionViolation::DuplicatePath { path: path.clone() });
            }
            if change.is_deleted {
                violations.push(AdmissionViolation::UnsupportedDeletion { path: path.clone() });
            }
            if is_protected_path(path) {
                violations.push(AdmissionViolation::ProtectedPath { path: path.clone() });
            } else if !permit.allowed_paths.is_empty()
                && !permit
                    .allowed_paths
                    .iter()
                    .any(|pattern| path_matches(pattern, path))
            {
                violations.push(AdmissionViolation::PathOutsidePermit { path: path.clone() });
            }
        }

        AdmissionReport {
            allowed: violations.is_empty(),
            violations,
            file_count: paths.len(),
            changed_lines,
            paths,
        }
    }
}

/// One independently checkable claim in an evidence capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCheck {
    pub name: String,
    pub passed: bool,
    pub details: String,
}

/// Audit artifact attached to every admitted draft PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCapsule {
    pub schema_version: u8,
    pub permit_id: String,
    pub repository: String,
    pub base_sha: String,
    pub contribution_fingerprint: String,
    pub generated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consent: ConsentSource,
    pub issue: Option<i64>,
    pub draft_only: bool,
    pub file_count: usize,
    pub changed_lines: usize,
    pub paths: Vec<String>,
    pub checks: Vec<EvidenceCheck>,
}

/// Stable reason why an evidence capsule cannot authorize a write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceViolation {
    UnsupportedSchema { actual: u8 },
    InvalidPermitId,
    RepositoryMismatch,
    MissingBaseRevision,
    InvalidBaseRevision,
    ContributionMismatch,
    ScopeMismatch,
    InvalidValidityWindow,
    Expired,
    NotDraftOnly,
    InvalidConsentSource,
    MissingAdmissionCheck,
    DuplicateCheck { name: String },
    FailedCheck { name: String },
    InvalidPath { path: String },
    DuplicatePath { path: String },
    UnsupportedDeletion { path: String },
}

impl std::fmt::Display for EvidenceViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema { actual } => {
                write!(formatter, "unsupported evidence schema version {actual}")
            }
            Self::InvalidPermitId => write!(formatter, "permit identifier is malformed"),
            Self::RepositoryMismatch => write!(formatter, "evidence belongs to another repository"),
            Self::MissingBaseRevision => write!(formatter, "evidence has no base revision"),
            Self::InvalidBaseRevision => write!(formatter, "evidence base is not a full Git SHA"),
            Self::ContributionMismatch => {
                write!(
                    formatter,
                    "evidence fingerprint does not match the contribution"
                )
            }
            Self::ScopeMismatch => {
                write!(formatter, "evidence scope does not match the contribution")
            }
            Self::InvalidValidityWindow => {
                write!(formatter, "evidence validity window is malformed")
            }
            Self::Expired => write!(formatter, "evidence capsule expired"),
            Self::NotDraftOnly => {
                write!(formatter, "evidence does not require a draft pull request")
            }
            Self::InvalidConsentSource => write!(formatter, "evidence consent source is invalid"),
            Self::MissingAdmissionCheck => write!(formatter, "admission policy check is missing"),
            Self::DuplicateCheck { name } => write!(formatter, "duplicate evidence check: {name}"),
            Self::FailedCheck { name } => write!(formatter, "evidence check failed: {name}"),
            Self::InvalidPath { path } => {
                write!(formatter, "evidence contains invalid path: {path}")
            }
            Self::DuplicatePath { path } => {
                write!(formatter, "evidence contains duplicate path: {path}")
            }
            Self::UnsupportedDeletion { path } => {
                write!(formatter, "evidence contains unsupported deletion: {path}")
            }
        }
    }
}

impl EvidenceCapsule {
    pub fn build(
        contribution: &Contribution,
        permit: &ContributionPermit,
        report: &AdmissionReport,
        mut checks: Vec<EvidenceCheck>,
    ) -> Self {
        checks.insert(
            0,
            EvidenceCheck {
                name: "admission_policy".to_string(),
                passed: report.allowed,
                details: if report.allowed {
                    "permit, scope, and protected-path checks passed".to_string()
                } else {
                    report
                        .violations
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                },
            },
        );

        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            permit_id: permit.id.clone(),
            repository: permit.repository.clone(),
            base_sha: permit.base_sha.clone(),
            contribution_fingerprint: contribution_fingerprint(contribution),
            generated_at: Utc::now(),
            expires_at: permit.expires_at,
            consent: permit.source.clone(),
            issue: permit.issue,
            draft_only: permit.draft_only,
            file_count: report.file_count,
            changed_lines: report.changed_lines,
            paths: report.paths.clone(),
            checks,
        }
    }

    /// Recompute every locally checkable claim before a GitHub write begins.
    ///
    /// This prevents a stale or unrelated capsule from being paired with a
    /// different repository or contribution after human review.
    pub fn validate_for_submission(
        &self,
        contribution: &Contribution,
        repository: &Repository,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), Vec<EvidenceViolation>> {
        let mut violations = Vec::new();
        if self.schema_version != EVIDENCE_SCHEMA_VERSION {
            violations.push(EvidenceViolation::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        if self.permit_id.len() != 24
            || !self.permit_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            violations.push(EvidenceViolation::InvalidPermitId);
        }
        if self.repository != repository.full_name {
            violations.push(EvidenceViolation::RepositoryMismatch);
        }
        if self.base_sha.trim().is_empty() {
            violations.push(EvidenceViolation::MissingBaseRevision);
        } else if !is_full_commit_sha(&self.base_sha) {
            violations.push(EvidenceViolation::InvalidBaseRevision);
        }
        if self.contribution_fingerprint != contribution_fingerprint(contribution) {
            violations.push(EvidenceViolation::ContributionMismatch);
        }
        if self.expires_at <= self.generated_at {
            violations.push(EvidenceViolation::InvalidValidityWindow);
        } else if now > self.expires_at {
            violations.push(EvidenceViolation::Expired);
        }
        if !self.draft_only {
            violations.push(EvidenceViolation::NotDraftOnly);
        }

        let source_is_valid = match &self.consent {
            ConsentSource::RepositoryManifest { path } => CONSENT_PATHS.contains(&path.as_str()),
            ConsentSource::MaintainerLabel { issue, label } => {
                self.issue == Some(*issue)
                    && MAINTAINER_APPROVAL_LABELS
                        .iter()
                        .any(|allowed| label.eq_ignore_ascii_case(allowed))
            }
        };
        if !source_is_valid {
            violations.push(EvidenceViolation::InvalidConsentSource);
        }

        let changes: Vec<&FileChange> = contribution
            .changes
            .iter()
            .chain(contribution.tests_added.iter())
            .collect();
        let paths: Vec<String> = changes.iter().map(|change| change.path.clone()).collect();
        let changed_lines = changes
            .iter()
            .map(|change| changed_line_count(change))
            .sum::<usize>();
        if self.file_count != paths.len()
            || self.changed_lines != changed_lines
            || self.paths != paths
        {
            violations.push(EvidenceViolation::ScopeMismatch);
        }

        let mut unique_paths = BTreeSet::new();
        for change in changes {
            if repository_path_error(&change.path).is_some() {
                violations.push(EvidenceViolation::InvalidPath {
                    path: change.path.clone(),
                });
            }
            if !unique_paths.insert(change.path.clone()) {
                violations.push(EvidenceViolation::DuplicatePath {
                    path: change.path.clone(),
                });
            }
            if change.is_deleted {
                violations.push(EvidenceViolation::UnsupportedDeletion {
                    path: change.path.clone(),
                });
            }
        }

        let mut check_names = BTreeSet::new();
        let mut has_admission_check = false;
        for check in &self.checks {
            if !check_names.insert(check.name.clone()) {
                violations.push(EvidenceViolation::DuplicateCheck {
                    name: check.name.clone(),
                });
            }
            if check.name == "admission_policy" {
                has_admission_check = true;
            }
            if !check.passed {
                violations.push(EvidenceViolation::FailedCheck {
                    name: check.name.clone(),
                });
            }
        }
        if !has_admission_check {
            violations.push(EvidenceViolation::MissingAdmissionCheck);
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Compact Markdown for the pull request description.
    pub fn to_markdown(&self) -> String {
        let consent = match &self.consent {
            ConsentSource::RepositoryManifest { path } => format!("repository manifest `{path}`"),
            ConsentSource::MaintainerLabel { issue, label } => {
                format!("maintainer label `{label}` on #{issue}")
            }
        };
        let checks = self
            .checks
            .iter()
            .map(|check| {
                format!(
                    "- [{}] **{}** — {}",
                    if check.passed { "x" } else { " " },
                    check.name,
                    check.details
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "## ContribAI Evidence\n\n\
             - **Permit**: `{}`\n\
             - **Consent**: {}\n\
             - **Base revision**: `{}`\n\
             - **Change fingerprint**: `{}`\n\
             - **Evidence expires**: `{}`\n\
             - **Scope**: {} files / {} changed lines\n\
             - **Submission mode**: draft only\n\n\
             {}",
            self.permit_id,
            consent,
            self.base_sha,
            self.contribution_fingerprint,
            self.expires_at.to_rfc3339(),
            self.file_count,
            self.changed_lines,
            checks
        )
    }
}

pub fn contribution_fingerprint(contribution: &Contribution) -> String {
    let mut digest = Sha256::new();
    hash_field(&mut digest, b"contribai-contribution-v2");
    hash_field(
        &mut digest,
        format!("{:?}", contribution.contribution_type).as_bytes(),
    );
    hash_field(&mut digest, contribution.title.as_bytes());
    hash_field(&mut digest, contribution.description.as_bytes());
    hash_field(&mut digest, contribution.commit_message.as_bytes());
    hash_field(&mut digest, contribution.branch_name.as_bytes());

    let finding = &contribution.finding;
    hash_field(
        &mut digest,
        format!("{:?}", finding.finding_type).as_bytes(),
    );
    hash_field(&mut digest, format!("{:?}", finding.severity).as_bytes());
    hash_field(&mut digest, finding.title.as_bytes());
    hash_field(&mut digest, finding.description.as_bytes());
    hash_field(&mut digest, finding.file_path.as_bytes());
    hash_field(&mut digest, format!("{:?}", finding.line_start).as_bytes());
    hash_field(&mut digest, format!("{:?}", finding.line_end).as_bytes());
    hash_field(&mut digest, format!("{:?}", finding.suggestion).as_bytes());
    hash_field(&mut digest, &finding.confidence.to_bits().to_be_bytes());
    hash_field(
        &mut digest,
        &(finding.priority_signals.len() as u64).to_be_bytes(),
    );
    for signal in &finding.priority_signals {
        hash_field(&mut digest, signal.as_bytes());
    }

    hash_changes(&mut digest, b"changes", &contribution.changes);
    hash_changes(&mut digest, b"tests_added", &contribution.tests_added);
    hex::encode(digest.finalize())
}

fn hash_changes(digest: &mut Sha256, section: &[u8], changes: &[FileChange]) {
    hash_field(digest, section);
    hash_field(digest, &(changes.len() as u64).to_be_bytes());
    for change in changes {
        hash_field(digest, change.path.as_bytes());
        match &change.original_content {
            Some(content) => {
                hash_field(digest, b"original:some");
                hash_field(digest, content.as_bytes());
            }
            None => hash_field(digest, b"original:none"),
        }
        hash_field(digest, change.new_content.as_bytes());
        hash_field(digest, &[u8::from(change.is_new_file)]);
        hash_field(digest, &[u8::from(change.is_deleted)]);
    }
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

pub fn changed_line_count(change: &FileChange) -> usize {
    let new_lines: Vec<&str> = change.new_content.lines().collect();
    let Some(original) = &change.original_content else {
        return new_lines.len();
    };
    let old_lines: Vec<&str> = original.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(old, new)| old == new)
        .count();
    let max_suffix = old_lines.len().min(new_lines.len()).saturating_sub(prefix);
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(max_suffix)
        .take_while(|(old, new)| old == new)
        .count();
    old_lines.len().saturating_sub(prefix + suffix)
        + new_lines.len().saturating_sub(prefix + suffix)
}

pub fn is_protected_path(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    matches!(
        file_name,
        "license"
            | "license.md"
            | "license.txt"
            | "contributing.md"
            | "code_of_conduct.md"
            | "security.md"
            | "codeowners"
            | "agents.md"
            | "ai_policy.md"
            | "contribai_allow"
            | "contribai_block"
    ) || normalized == ".github/contribai.yml"
        || normalized == ".github/contribai.yaml"
        || normalized.starts_with(".github/workflows/")
        || normalized == ".github/funding.yml"
}

/// Return why a generated repository path is not a canonical relative POSIX path.
pub fn repository_path_error(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("path is empty");
    }
    if path.starts_with('/') {
        return Some("absolute paths are forbidden");
    }
    if path.contains('\\') {
        return Some("backslash separators are forbidden");
    }
    if path.contains(['%', '?', '#']) {
        return Some("URI metacharacters are forbidden");
    }
    if path.chars().any(char::is_control) {
        return Some("control characters are forbidden");
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Some("empty, dot, and parent components are forbidden");
    }
    None
}

/// GitHub currently exposes full SHA-1 object IDs and may expose SHA-256 IDs.
pub fn is_full_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_safe_allow_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.starts_with('/')
        && !pattern.contains('\\')
        && !pattern.contains(['%', '?', '#'])
        && !pattern.chars().any(char::is_control)
        && !pattern
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        && Pattern::new(pattern).is_ok()
}

fn path_matches(pattern: &str, path: &str) -> bool {
    Pattern::new(pattern)
        .map(|compiled| {
            compiled.matches_with(
                path,
                MatchOptions {
                    case_sensitive: true,
                    require_literal_separator: true,
                    require_literal_leading_dot: true,
                },
            )
        })
        .unwrap_or(false)
}

fn short_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)[..24].to_string()
}

/// Return unique paths in deterministic order, useful to external policy adapters.
pub fn contribution_paths(contribution: &Contribution) -> Vec<String> {
    contribution
        .changes
        .iter()
        .chain(contribution.tests_added.iter())
        .map(|change| change.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{ContributionType, Finding, Severity};

    const TEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repository(name: &str) -> Repository {
        Repository {
            owner: name.split('/').next().unwrap_or("owner").to_string(),
            name: name.split('/').nth(1).unwrap_or("repo").to_string(),
            full_name: name.to_string(),
            description: None,
            language: Some("Rust".to_string()),
            languages: Default::default(),
            stars: 0,
            forks: 0,
            open_issues: 0,
            topics: Vec::new(),
            default_branch: "main".to_string(),
            html_url: String::new(),
            clone_url: String::new(),
            has_contributing: false,
            has_license: true,
            last_push_at: None,
            created_at: None,
        }
    }

    fn contribution(path: &str, original: Option<&str>, new_content: &str) -> Contribution {
        Contribution {
            finding: Finding {
                id: "f1".to_string(),
                finding_type: ContributionType::CodeQuality,
                severity: Severity::Medium,
                title: "Fix parser".to_string(),
                description: "Parser bug".to_string(),
                file_path: path.to_string(),
                line_start: Some(1),
                line_end: Some(1),
                suggestion: None,
                confidence: 0.9,
                priority_signals: Vec::new(),
            },
            contribution_type: ContributionType::CodeQuality,
            title: "fix: parser".to_string(),
            description: "Fix parser".to_string(),
            changes: vec![FileChange {
                path: path.to_string(),
                original_content: original.map(str::to_string),
                new_content: new_content.to_string(),
                is_new_file: original.is_none(),
                is_deleted: false,
            }],
            commit_message: "fix: parser".to_string(),
            tests_added: Vec::new(),
            branch_name: String::new(),
            generated_at: Utc::now(),
        }
    }

    fn consent(paths: &[&str]) -> RepositoryConsent {
        RepositoryConsent {
            source: ConsentSource::RepositoryManifest {
                path: CONSENT_PATHS[0].to_string(),
            },
            max_files: 5,
            max_changed_lines: 250,
            allowed_paths: paths.iter().map(|path| path.to_string()).collect(),
            draft_only: true,
        }
    }

    #[test]
    fn manifest_requires_explicit_true() {
        assert!(RepositoryConsent::parse(CONSENT_PATHS[0], "max_files: 2").is_none());
        assert!(RepositoryConsent::parse(CONSENT_PATHS[0], "enabled: false").is_none());
        assert!(RepositoryConsent::parse(CONSENT_PATHS[0], "enabled: true").is_some());
    }

    #[test]
    fn manifest_parses_budgets_and_paths() {
        let parsed = RepositoryConsent::parse(
            CONSENT_PATHS[0],
            "enabled: true\nmax_files: 2\nmax_changed_lines: 80\nallowed_paths: src/**, tests/**",
        )
        .expect("valid consent");
        assert_eq!(parsed.max_files, 2);
        assert_eq!(parsed.max_changed_lines, 80);
        assert_eq!(parsed.allowed_paths, vec!["src/**", "tests/**"]);
        assert!(parsed.draft_only);
    }

    #[test]
    fn canonical_yaml_manifest_accepts_path_lists() {
        let parsed = RepositoryConsent::parse(
            ".github/contribai.yml",
            "schema_version: 1\nenabled: true\nallowed_paths:\n  - src/**\n  - tests/**\n",
        )
        .expect("valid consent");
        assert_eq!(parsed.allowed_paths, vec!["src/**", "tests/**"]);
    }

    #[test]
    fn manifest_rejects_zero_budgets_and_malformed_yaml() {
        assert!(
            RepositoryConsent::parse(".github/contribai.yml", "enabled: true\nmax_files: 0")
                .is_none()
        );
        assert!(RepositoryConsent::parse(".github/contribai.yml", "enabled: [true").is_none());
    }

    #[test]
    fn manifest_rejects_unknown_schema_fields_and_unsafe_patterns() {
        assert!(RepositoryConsent::parse(
            ".github/contribai.yml",
            "schema_version: 2\nenabled: true",
        )
        .is_none());
        assert!(RepositoryConsent::parse(
            ".github/contribai.yml",
            "schema_version: 1\nenabled: true\nmax_file: 1",
        )
        .is_none());
        for pattern in ["../**", "src\\**", "src/[", "src//**"] {
            let content = format!("schema_version: 1\nenabled: true\nallowed_paths: ['{pattern}']");
            assert!(
                RepositoryConsent::parse(".github/contribai.yml", &content).is_none(),
                "unsafe pattern {pattern:?} must deny consent"
            );
        }
    }

    #[test]
    fn issue_consent_requires_maintainer_label() {
        let mut issue = Issue {
            number: 42,
            title: "Bug".to_string(),
            body: None,
            labels: vec!["bug".to_string()],
            state: "open".to_string(),
            created_at: None,
            html_url: String::new(),
        };
        assert!(RepositoryConsent::from_issue(&issue).is_none());
        issue.labels.push("agent-ready".to_string());
        assert!(matches!(
            RepositoryConsent::from_issue(&issue).map(|value| value.source),
            Some(ConsentSource::MaintainerLabel { issue: 42, .. })
        ));
        issue.state = "closed".to_string();
        assert!(RepositoryConsent::from_issue(&issue).is_none());
    }

    #[test]
    fn admission_allows_scoped_change() {
        let repo = repository("owner/repo");
        let change = contribution("src/parser.rs", Some("old\n"), "new\n");
        let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&["src/**"]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        assert!(report.allowed, "{:?}", report.violations);
        assert_eq!(report.changed_lines, 2);
    }

    #[test]
    fn admission_blocks_governance_and_out_of_scope_paths() {
        let repo = repository("owner/repo");
        for path in ["AGENTS.md", ".github/contribai.yml"] {
            let change = contribution(path, Some("old"), "new");
            let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&["**"]), None);
            let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
            assert!(!report.allowed, "{path} must remain protected");
            assert!(report
                .violations
                .iter()
                .any(|violation| matches!(violation, AdmissionViolation::ProtectedPath { .. })));
        }
    }

    #[test]
    fn admission_blocks_expired_and_cross_repo_permits() {
        let repo = repository("owner/repo");
        let other = repository("other/repo");
        let change = contribution("src/lib.rs", Some("old"), "new");
        let mut permit = ContributionPermit::issue(&other, TEST_SHA, consent(&[]), None);
        permit.expires_at = Utc::now() - Duration::seconds(1);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        assert!(report
            .violations
            .contains(&AdmissionViolation::RepositoryMismatch));
        assert!(report
            .violations
            .contains(&AdmissionViolation::PermitExpired));
    }

    #[test]
    fn admission_rejects_noncanonical_duplicate_and_deleted_paths() {
        let repo = repository("owner/repo");
        for path in [
            "../SECURITY.md",
            "src/../.github/workflows/release.yml",
            "/src/lib.rs",
            "src\\lib.rs",
            "src//lib.rs",
            "src/%2e%2e/SECURITY.md",
        ] {
            let change = contribution(path, Some("old"), "new");
            let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&["**"]), None);
            let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
            assert!(
                report
                    .violations
                    .iter()
                    .any(|violation| matches!(violation, AdmissionViolation::InvalidPath { .. })),
                "unsafe path {path:?} must be rejected: {:?}",
                report.violations
            );
        }

        let mut duplicate = contribution("src/lib.rs", Some("old"), "new");
        duplicate.tests_added = duplicate.changes.clone();
        let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&["src/**"]), None);
        let report = AdmissionController::evaluate(&repo, &duplicate, &permit, Utc::now());
        assert!(report
            .violations
            .iter()
            .any(|violation| matches!(violation, AdmissionViolation::DuplicatePath { .. })));

        let mut deletion = contribution("src/lib.rs", Some("old"), "");
        deletion.changes[0].is_deleted = true;
        let report = AdmissionController::evaluate(&repo, &deletion, &permit, Utc::now());
        assert!(report
            .violations
            .iter()
            .any(|violation| matches!(violation, AdmissionViolation::UnsupportedDeletion { .. })));
    }

    #[test]
    fn allowlist_globs_do_not_cross_unmatched_directory_boundaries() {
        let repo = repository("owner/repo");
        let nested = contribution("src/parser/mod.rs", Some("old"), "new");
        let shallow = ContributionPermit::issue(&repo, TEST_SHA, consent(&["src/*"]), None);
        let recursive = ContributionPermit::issue(&repo, TEST_SHA, consent(&["src/**"]), None);
        assert!(!AdmissionController::evaluate(&repo, &nested, &shallow, Utc::now()).allowed);
        assert!(AdmissionController::evaluate(&repo, &nested, &recursive, Utc::now()).allowed);
    }

    #[test]
    fn admission_requires_a_full_commit_object_id() {
        let repo = repository("owner/repo");
        let change = contribution("src/lib.rs", Some("old"), "new");
        let permit = ContributionPermit::issue(&repo, "abc123", consent(&[]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        assert!(report
            .violations
            .contains(&AdmissionViolation::InvalidBaseRevision));
    }

    #[test]
    fn permit_identifier_binds_path_scope() {
        let repo = repository("owner/repo");
        let source = ContributionPermit::issue(&repo, TEST_SHA, consent(&["src/**"]), None);
        let docs = ContributionPermit::issue(&repo, TEST_SHA, consent(&["docs/**"]), None);
        assert_ne!(source.id, docs.id);
    }

    #[test]
    fn changed_line_count_trims_common_prefix_and_suffix() {
        let change = FileChange {
            path: "src/lib.rs".to_string(),
            original_content: Some("same\nold\ntail\n".to_string()),
            new_content: "same\nnew\ntail\n".to_string(),
            is_new_file: false,
            is_deleted: false,
        };
        assert_eq!(changed_line_count(&change), 2);
    }

    #[test]
    fn evidence_is_deterministic_for_same_change() {
        let first = contribution("src/lib.rs", Some("old"), "new");
        let second = first.clone();
        assert_eq!(
            contribution_fingerprint(&first),
            contribution_fingerprint(&second)
        );
    }

    #[test]
    fn evidence_markdown_discloses_consent_and_attestation() {
        let repo = repository("owner/repo");
        let change = contribution("src/lib.rs", Some("old"), "new");
        let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&[]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        let capsule = EvidenceCapsule::build(&change, &permit, &report, Vec::new());
        let markdown = capsule.to_markdown();
        assert!(markdown.contains(&permit.id));
        assert!(markdown.contains(TEST_SHA));
        assert!(markdown.contains("draft only"));
    }

    #[test]
    fn evidence_validation_binds_the_exact_candidate_and_repository() {
        let repo = repository("owner/repo");
        let change = contribution("src/lib.rs", Some("old"), "new");
        let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&[]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        let capsule = EvidenceCapsule::build(&change, &permit, &report, Vec::new());
        assert!(capsule
            .validate_for_submission(&change, &repo, Utc::now())
            .is_ok());

        let mut tampered = change.clone();
        tampered.commit_message = "chore: unrelated rewrite".to_string();
        let violations = capsule
            .validate_for_submission(&tampered, &repo, Utc::now())
            .expect_err("mutated candidate must not reuse reviewed evidence");
        assert!(violations.contains(&EvidenceViolation::ContributionMismatch));

        let other = repository("other/repo");
        let violations = capsule
            .validate_for_submission(&change, &other, Utc::now())
            .expect_err("cross-repository evidence must be rejected");
        assert!(violations.contains(&EvidenceViolation::RepositoryMismatch));

        let issue_permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&[]), Some(42));
        let issue_report = AdmissionController::evaluate(&repo, &change, &issue_permit, Utc::now());
        let issue_capsule =
            EvidenceCapsule::build(&change, &issue_permit, &issue_report, Vec::new());
        assert!(issue_capsule
            .validate_for_submission(&change, &repo, Utc::now())
            .is_ok());
    }

    #[test]
    fn evidence_validation_rejects_expired_or_failed_capsules() {
        let repo = repository("owner/repo");
        let change = contribution("src/lib.rs", Some("old"), "new");
        let permit = ContributionPermit::issue(&repo, TEST_SHA, consent(&[]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        let mut capsule = EvidenceCapsule::build(&change, &permit, &report, Vec::new());
        capsule.generated_at = Utc::now() - Duration::hours(2);
        capsule.expires_at = Utc::now() - Duration::hours(1);
        let violations = capsule
            .validate_for_submission(&change, &repo, Utc::now())
            .expect_err("expired evidence must be rejected");
        assert!(violations.contains(&EvidenceViolation::Expired));

        capsule.expires_at = Utc::now() + Duration::hours(1);
        capsule.checks[0].passed = false;
        let violations = capsule
            .validate_for_submission(&change, &repo, Utc::now())
            .expect_err("failed evidence must be rejected");
        assert!(violations.contains(&EvidenceViolation::FailedCheck {
            name: "admission_policy".to_string(),
        }));
    }
}
