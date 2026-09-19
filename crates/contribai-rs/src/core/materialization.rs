//! Workspace materialization accounting.
//!
//! Materializing the run workspace is *not* the same thing as building model
//! context. The workspace must contain enough of the repository — at the
//! attested base SHA — for deterministic validation to be meaningful. A
//! snapshot truncated by a file-count or per-file-size limit is not a full
//! checkout, and this module makes that distinction explicit and checkable.
//!
//! Every file that was not materialized is recorded with a reason, and the
//! executor fails closed when required evidence-bearing files are missing —
//! it never continues and calls an incomplete tree a valid test workspace.

use serde::{Deserialize, Serialize};

use super::admission::{is_dependency_manifest, is_lockfile, is_test_path, ContributionPermit};

/// Why a repository file was not materialized into the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// File exceeded the per-file byte cap for API-fetched snapshots.
    Oversized,
    /// Binary/non-reviewable extension deliberately excluded.
    BinaryExtension,
    /// The fetch of this file's content failed.
    FetchError,
    /// The tree listing exceeded the file-count limit; this file and all
    /// entries after it were never fetched.
    OverLimit,
    /// The listing itself was truncated by the host (e.g. GitHub caps
    /// recursive tree responses).
    ListingTruncated,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::BinaryExtension => "binary_extension",
            Self::FetchError => "fetch_error",
            Self::OverLimit => "over_limit",
            Self::ListingTruncated => "listing_truncated",
        }
    }
}

/// A repository file that was not materialized, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
    /// Reported size when known (0 when the host did not report one).
    #[serde(default)]
    pub size: i64,
}

/// Result of [`RunEnvironment::fetch_base_snapshot`](crate::orchestrator::run_executor::RunEnvironment::fetch_base_snapshot).
#[derive(Debug, Clone, Default)]
pub struct BaseSnapshot {
    /// `(repo-relative path, content bytes)` pairs successfully fetched.
    pub files: Vec<(String, Vec<u8>)>,
    /// Files present in the tree but not materialized, with reasons.
    pub skipped: Vec<SkippedFile>,
    /// True when the file listing or fetch was cut short — the snapshot is
    /// provably not a complete checkout.
    pub truncated: bool,
}

/// How a workspace was materialized and how complete it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializationReport {
    /// `worktree`, `clone`, or `snapshot`.
    pub strategy: String,
    /// Files written into the workspace.
    pub materialized_files: usize,
    /// Files known to the source but not materialized.
    pub skipped: Vec<SkippedFile>,
    /// Whether the source listing was truncated before fetching finished.
    pub truncated: bool,
    /// Non-empty when the workspace is provably incomplete. Each entry is a
    /// bounded human-readable reason the run must not call tests valid.
    pub gaps: Vec<String>,
}

impl MaterializationReport {
    /// The workspace covers everything the run can prove it needs.
    pub fn is_complete(&self) -> bool {
        self.gaps.is_empty()
    }
}

/// A materialized workspace plus its completeness report.
pub struct MaterializedWorkspace {
    pub workspace: crate::exec::workspace::RunWorkspace,
    pub report: MaterializationReport,
}

/// Deterministic materialization policy: which gaps block the run.
///
/// - A truncated listing or any fetch error means the tree is unknown —
///   the run cannot prove validation covered the real project.
/// - A skipped file inside the permit's `allowed_paths` scope blocks: the
///   solver is expected to edit in scope it cannot see.
/// - A skipped dependency manifest or lockfile blocks: ecosystem detection
///   and builds silently degrade without it.
/// - A skipped test file blocks when `allow_test_changes` — the permit
///   expects test evidence the workspace cannot run.
/// - Binary-extension skips outside scope are recorded but not blocking.
pub fn materialization_gaps(
    report: &MaterializationReport,
    permit: &ContributionPermit,
) -> Vec<String> {
    let mut gaps = Vec::new();
    if report.truncated {
        gaps.push(
            "file listing was truncated; workspace is not a complete checkout of the attested base"
                .to_string(),
        );
    }
    let fetch_errors = report
        .skipped
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::FetchError | SkipReason::OverLimit))
        .count();
    if fetch_errors > 0 {
        gaps.push(format!(
            "{fetch_errors} file(s) could not be fetched into the workspace"
        ));
    }
    for skip in &report.skipped {
        let in_scope = !permit.allowed_paths.is_empty()
            && permit
                .allowed_paths
                .iter()
                .any(|pattern| crate::core::admission::path_matches(pattern, &skip.path));
        if in_scope {
            gaps.push(format!(
                "in-scope file {} was not materialized ({})",
                skip.path,
                skip.reason.as_str()
            ));
            continue;
        }
        if is_dependency_manifest(&skip.path) || is_lockfile(&skip.path) {
            gaps.push(format!(
                "dependency manifest {} was not materialized ({})",
                skip.path,
                skip.reason.as_str()
            ));
            continue;
        }
        if permit.allow_test_changes && is_test_path(&skip.path) {
            gaps.push(format!(
                "test file {} was not materialized ({})",
                skip.path,
                skip.reason.as_str()
            ));
        }
    }
    gaps.sort();
    gaps.dedup();
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::admission::{ConsentSource, ExecutionMode, RepositoryConsent};
    use crate::core::models::Repository;

    fn permit(allowed: &[&str]) -> ContributionPermit {
        let repo = Repository {
            owner: "o".into(),
            name: "r".into(),
            full_name: "o/r".into(),
            description: None,
            language: None,
            languages: Default::default(),
            stars: 0,
            forks: 0,
            open_issues: 0,
            default_branch: "main".into(),
            topics: vec![],
            html_url: String::new(),
            clone_url: String::new(),
            has_contributing: false,
            has_license: true,
            last_push_at: None,
            created_at: None,
        };
        let consent = RepositoryConsent {
            source: ConsentSource::RepositoryManifest {
                path: ".github/contribai.yml".into(),
            },
            schema_version: 2,
            max_files: 5,
            max_changed_lines: 250,
            allowed_paths: allowed.iter().map(|s| s.to_string()).collect(),
            denied_paths: vec![],
            required_checks: vec![],
            allow_dependency_changes: false,
            allow_new_files: true,
            allow_test_changes: true,
            required_reproduction: false,
            execution_mode: ExecutionMode::Local,
            max_runtime_seconds: 900,
            allowed_issue_labels: vec![],
            draft_only: true,
        };
        ContributionPermit::issue(&repo, "0".repeat(40), consent, Some(1))
    }

    fn report(skipped: Vec<SkippedFile>, truncated: bool) -> MaterializationReport {
        MaterializationReport {
            strategy: "snapshot".into(),
            materialized_files: 10,
            skipped,
            truncated,
            gaps: Vec::new(),
        }
    }

    #[test]
    fn complete_snapshot_has_no_gaps() {
        let r = report(vec![], false);
        assert!(materialization_gaps(&r, &permit(&["src/**"])).is_empty());
    }

    #[test]
    fn truncated_listing_always_blocks() {
        let r = report(vec![], true);
        assert!(!materialization_gaps(&r, &permit(&[])).is_empty());
    }

    #[test]
    fn fetch_errors_always_block() {
        let r = report(
            vec![SkippedFile {
                path: "assets/logo.png".into(),
                reason: SkipReason::FetchError,
                size: 10,
            }],
            false,
        );
        assert!(!materialization_gaps(&r, &permit(&["src/**"])).is_empty());
    }

    #[test]
    fn in_scope_skips_block_out_of_scope_binaries_do_not() {
        let p = permit(&["src/**"]);
        let in_scope = report(
            vec![SkippedFile {
                path: "src/big.rs".into(),
                reason: SkipReason::Oversized,
                size: 200_000,
            }],
            false,
        );
        assert!(!materialization_gaps(&in_scope, &p).is_empty());

        let out_of_scope = report(
            vec![SkippedFile {
                path: "assets/logo.png".into(),
                reason: SkipReason::BinaryExtension,
                size: 50_000,
            }],
            false,
        );
        assert!(materialization_gaps(&out_of_scope, &p).is_empty());
    }

    #[test]
    fn skipped_manifests_and_tests_block() {
        let p = permit(&[]);
        let r = report(
            vec![
                SkippedFile {
                    path: "Cargo.toml".into(),
                    reason: SkipReason::Oversized,
                    size: 90_000,
                },
                SkippedFile {
                    path: "tests/integration.rs".into(),
                    reason: SkipReason::BinaryExtension,
                    size: 1,
                },
            ],
            false,
        );
        let gaps = materialization_gaps(&r, &p);
        assert_eq!(gaps.len(), 2);
    }
}
