//! Maintainer-controlled admission for AI-mediated contributions.
//!
//! This module deliberately separates generating a change from earning permission
//! to publish it. A contribution is publishable only when it carries an explicit,
//! time-bounded permit rooted in repository or maintainer consent and passes the
//! scope policy. The resulting evidence capsule is deterministic and auditable.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use glob::Pattern;
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
    /// are ignored so the format can evolve without making older clients unsafe.
    pub fn parse(path: &str, content: &str) -> Option<Self> {
        let manifest: ConsentManifest = serde_yaml::from_str(content).ok()?;
        if !manifest.enabled {
            return None;
        }
        let max_files = manifest.max_files.unwrap_or(DEFAULT_MAX_FILES);
        let max_changed_lines = manifest
            .max_changed_lines
            .unwrap_or(DEFAULT_MAX_CHANGED_LINES);
        if max_files == 0 || max_changed_lines == 0 {
            return None;
        }

        Some(Self {
            source: ConsentSource::RepositoryManifest {
                path: path.to_string(),
            },
            max_files,
            max_changed_lines,
            allowed_paths: manifest.allowed_paths.into_paths(),
            draft_only: true,
        })
    }

    /// Construct consent from a maintainer-controlled issue label.
    pub fn from_issue(issue: &Issue) -> Option<Self> {
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
struct ConsentManifest {
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
            "v1\n{}\n{}\n{:?}\n{:?}\n{}\n{}\n{}",
            self.repository,
            self.base_sha,
            self.source,
            self.issue,
            self.max_files,
            self.max_changed_lines,
            self.issued_at.timestamp()
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
    PermitExpired,
    RepositoryMismatch,
    TooManyFiles { actual: usize, maximum: usize },
    TooManyChangedLines { actual: usize, maximum: usize },
    ProtectedPath { path: String },
    PathOutsidePermit { path: String },
}

impl std::fmt::Display for AdmissionViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExternalWritesNotEnabled => write!(formatter, "external writes were not enabled"),
            Self::MissingConsent => write!(formatter, "maintainer consent was not found"),
            Self::MissingBaseRevision => write!(formatter, "base revision could not be attested"),
            Self::PermitExpired => write!(formatter, "contribution permit expired"),
            Self::RepositoryMismatch => write!(formatter, "permit belongs to another repository"),
            Self::TooManyFiles { actual, maximum } => {
                write!(formatter, "changed {actual} files; permit allows {maximum}")
            }
            Self::TooManyChangedLines { actual, maximum } => {
                write!(formatter, "changed {actual} lines; permit allows {maximum}")
            }
            Self::ProtectedPath { path } => write!(formatter, "protected path: {path}"),
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

        for path in &paths {
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
    pub consent: ConsentSource,
    pub issue: Option<i64>,
    pub draft_only: bool,
    pub file_count: usize,
    pub changed_lines: usize,
    pub paths: Vec<String>,
    pub checks: Vec<EvidenceCheck>,
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
            schema_version: 1,
            permit_id: permit.id.clone(),
            repository: permit.repository.clone(),
            base_sha: permit.base_sha.clone(),
            contribution_fingerprint: contribution_fingerprint(contribution),
            generated_at: Utc::now(),
            consent: permit.source.clone(),
            issue: permit.issue,
            draft_only: permit.draft_only,
            file_count: report.file_count,
            changed_lines: report.changed_lines,
            paths: report.paths.clone(),
            checks,
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
             - **Scope**: {} files / {} changed lines\n\
             - **Submission mode**: draft only\n\n\
             {}",
            self.permit_id,
            consent,
            self.base_sha,
            self.contribution_fingerprint,
            self.file_count,
            self.changed_lines,
            checks
        )
    }
}

pub fn contribution_fingerprint(contribution: &Contribution) -> String {
    let mut material = String::new();
    material.push_str(&contribution.title);
    material.push('\n');
    let mut changes: Vec<&FileChange> = contribution
        .changes
        .iter()
        .chain(contribution.tests_added.iter())
        .collect();
    changes.sort_by_key(|change| &change.path);
    for change in changes {
        material.push_str(&change.path);
        material.push('\0');
        material.push_str(&change.new_content);
        material.push('\0');
    }
    short_sha256(material.as_bytes())
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
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
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

fn path_matches(pattern: &str, path: &str) -> bool {
    Pattern::new(pattern)
        .map(|compiled| compiled.matches_path(std::path::Path::new(path)))
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
    }

    #[test]
    fn admission_allows_scoped_change() {
        let repo = repository("owner/repo");
        let change = contribution("src/parser.rs", Some("old\n"), "new\n");
        let permit = ContributionPermit::issue(&repo, "abc123", consent(&["src/**"]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        assert!(report.allowed, "{:?}", report.violations);
        assert_eq!(report.changed_lines, 2);
    }

    #[test]
    fn admission_blocks_governance_and_out_of_scope_paths() {
        let repo = repository("owner/repo");
        for path in ["AGENTS.md", ".github/contribai.yml"] {
            let change = contribution(path, Some("old"), "new");
            let permit = ContributionPermit::issue(&repo, "abc123", consent(&["**"]), None);
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
        let mut permit = ContributionPermit::issue(&other, "abc123", consent(&[]), None);
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
        let permit = ContributionPermit::issue(&repo, "abc123", consent(&[]), None);
        let report = AdmissionController::evaluate(&repo, &change, &permit, Utc::now());
        let capsule = EvidenceCapsule::build(&change, &permit, &report, Vec::new());
        let markdown = capsule.to_markdown();
        assert!(markdown.contains(&permit.id));
        assert!(markdown.contains("abc123"));
        assert!(markdown.contains("draft only"));
    }
}
