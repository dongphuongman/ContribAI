//! Review-cost estimation.
//!
//! Reviewer attention is the scarce resource. This module computes a
//! deterministic, explainable estimate of how much surface a maintainer must
//! verify — files, lines, subsystem spread, dependency and security-sensitive
//! touches, generated/binary churn — plus the review-budget fields carried by
//! the evidence layer.
//!
//! This is deliberately NOT a quality score. It never claims the patch is
//! good; it only describes what review costs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

use crate::core::admission::{is_dependency_manifest, is_lockfile, is_test_path};
use crate::core::models::FileChange;

/// One changed file as seen by the review-surface estimator.
///
/// For workspace-backed runs these counts come from `git diff --numstat`
/// (authoritative). For API-produced `FileChange`s they are estimated by a
/// deterministic multiset line diff — an approximation, labelled as such.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    pub lines_added: usize,
    pub lines_deleted: usize,
    /// `None` means text; `Some(ext)` marks a non-reviewable binary blob.
    pub is_binary: bool,
}

impl ChangedFile {
    /// Build from a `FileChange` using a deterministic multiset line diff.
    /// Exact for add/delete; an estimate for in-place modifications.
    pub fn from_file_change(change: &FileChange) -> Self {
        let (added, deleted) = line_diff_counts(
            change.original_content.as_deref().unwrap_or(""),
            &change.new_content,
        );
        let is_binary =
            change.new_content.as_bytes().contains(&0) || looks_binary_extension(&change.path);
        Self {
            path: change.path.replace('\\', "/"),
            lines_added: added,
            lines_deleted: deleted,
            is_binary,
        }
    }
}

/// Multiset line diff: counts of lines present in `after` but not `before`
/// (added) and vice versa (deleted). Deterministic; cheap; good enough for
/// a review-cost estimate — NOT used for admission scope checks.
pub fn line_diff_counts(before: &str, after: &str) -> (usize, usize) {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for line in before.lines() {
        *counts.entry(line).or_default() -= 1;
    }
    for line in after.lines() {
        *counts.entry(line).or_default() += 1;
    }
    let mut added = 0usize;
    let mut deleted = 0usize;
    for count in counts.values() {
        if *count > 0 {
            added += *count as usize;
        } else {
            deleted += (-*count) as usize;
        }
    }
    (added, deleted)
}

fn looks_binary_extension(path: &str) -> bool {
    let lowered = path.to_lowercase();
    const EXT: &[&str] = &[
        ".bin", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".wasm", ".so", ".dll", ".dylib",
        ".exe", ".zip", ".gz", ".tar", ".xz", ".7z", ".pdf", ".woff", ".woff2", ".ttf", ".otf",
        ".eot", ".mp3", ".mp4", ".mov", ".jar", ".class", ".o", ".a", ".pyc", ".pyo", ".db",
        ".sqlite", ".parquet",
    ];
    EXT.iter().any(|ext| lowered.ends_with(ext))
}

/// Path heuristics — deterministic, conservative, documented.
fn is_security_sensitive(path: &str) -> bool {
    let lowered = path.to_lowercase();
    const NEEDLES: &[&str] = &[
        "auth",
        "crypto",
        "token",
        "secret",
        "password",
        "cred",
        "session",
        "cookie",
        "oauth",
        "permission",
        "acl",
        "sanitiz",
        "escape",
        "inject",
        "xss",
        "csrf",
        "encrypt",
        "decrypt",
        "tls",
        "ssl",
        "cert",
        "sign",
        "verify",
        "hash",
    ];
    NEEDLES.iter().any(|needle| lowered.contains(needle))
}

/// Governance-adjacent paths reviewers should see flagged explicitly.
/// These mirror (but do not replace) the enforced protected-path policy.
fn is_governance_adjacent(path: &str) -> bool {
    let lowered = path.to_lowercase();
    lowered.starts_with(".github/")
        || lowered.starts_with(".devin/")
        || lowered.starts_with(".agents/")
        || lowered.contains("contribai")
        || lowered.ends_with("/security.md")
        || lowered == "security.md"
        || lowered.ends_with("/contributing.md")
        || lowered == "contributing.md"
        || lowered.ends_with("/codeowners")
        || lowered == "codeowners"
}

/// Subsystem key for spread measurement: `src/core/…` → `src/core`.
fn subsystem_key(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        [single] => (*single).to_string(),
        [first, second, ..] => format!("{first}/{second}"),
    }
}

/// Inputs that do not come from the change list itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewContext {
    /// Deterministic validations executed (graph node count).
    pub validation_count: usize,
    /// Unresolved high/critical challenger findings.
    pub unresolved_concerns: usize,
    /// Repair iterations performed before this candidate.
    pub repair_iterations: usize,
}

/// The deterministic review-surface estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewSurface {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_deleted: usize,
    /// Distinct two-level components touched (`src/core` + `src/llm` = 2).
    pub subsystem_count: usize,
    /// Paths matching security-sensitive heuristics (auth/crypto/token/…).
    pub security_sensitive_paths: usize,
    /// Manifest/lockfile changes (Cargo.toml, package.json, go.mod, …).
    pub dependency_changes: usize,
    /// Test-file changes — for test-to-code context.
    pub test_files_changed: usize,
    /// Lockfile or vendored/generated churn reviewers rarely read fully.
    pub generated_or_lockfile_changes: usize,
    /// Binary / non-reviewable artifacts.
    pub binary_changes: usize,
    /// Governance-adjacent files — should be zero on an admitted candidate.
    pub governance_paths: usize,
    /// Deterministic validations executed.
    pub validation_count: usize,
    /// Unresolved high/critical challenger findings.
    pub unresolved_concerns: usize,
    /// Repair iterations performed before this candidate.
    pub repair_iterations: usize,
    /// Human-readable budget summary lines.
    pub summary_lines: Vec<String>,
    /// Fingerprint over the normalized surface (binds evidence to it).
    pub surface_fingerprint: String,
}

impl ReviewSurface {
    /// Compute the surface over the candidate's file changes.
    pub fn compute(changes: &[ChangedFile], context: ReviewContext) -> Self {
        let mut subsystems: BTreeSet<String> = BTreeSet::new();
        let mut lines_added = 0usize;
        let mut lines_deleted = 0usize;
        let mut security = 0usize;
        let mut deps = 0usize;
        let mut tests = 0usize;
        let mut generated = 0usize;
        let mut binary = 0usize;
        let mut governance = 0usize;

        for change in changes {
            let normalized = change.path.replace('\\', "/");
            subsystems.insert(subsystem_key(&normalized));
            lines_added += change.lines_added;
            lines_deleted += change.lines_deleted;
            if change.is_binary {
                binary += 1;
            }
            if is_security_sensitive(&normalized) {
                security += 1;
            }
            if is_dependency_manifest(&normalized) {
                deps += 1;
            }
            if is_test_path(&normalized) {
                tests += 1;
            }
            if is_lockfile(&normalized) {
                generated += 1;
            }
            if is_governance_adjacent(&normalized) {
                governance += 1;
            }
        }

        let files_changed = changes.len();
        let mut summary_lines = vec![
            format!("{files_changed} files"),
            format!("{} changed lines", lines_added + lines_deleted),
        ];
        if security > 0 {
            summary_lines.push(format!("{security} security-sensitive path(s)"));
        }
        if deps > 0 {
            summary_lines.push(format!("{deps} dependency manifest change(s)"));
        }
        if governance > 0 {
            summary_lines.push(format!("{governance} governance-adjacent path(s)"));
        }
        if binary > 0 {
            summary_lines.push(format!("{binary} binary/non-reviewable file(s)"));
        }
        summary_lines.push(format!(
            "{} deterministic validations",
            context.validation_count
        ));
        if context.unresolved_concerns > 0 {
            summary_lines.push(format!(
                "{} unresolved challenger warning(s)",
                context.unresolved_concerns
            ));
        }
        if context.repair_iterations > 0 {
            summary_lines.push(format!("{} repair iteration(s)", context.repair_iterations));
        }

        let mut digest = Sha256::new();
        digest.update(b"contribai-review-surface-v1");
        digest.update(files_changed.to_le_bytes());
        digest.update(lines_added.to_le_bytes());
        digest.update(lines_deleted.to_le_bytes());
        for subsystem in &subsystems {
            digest.update(subsystem.as_bytes());
            digest.update([0]);
        }
        for value in [
            security,
            deps,
            tests,
            generated,
            binary,
            governance,
            context.validation_count,
            context.unresolved_concerns,
            context.repair_iterations,
        ] {
            digest.update(value.to_le_bytes());
        }
        let surface_fingerprint = hex::encode(digest.finalize());

        Self {
            files_changed,
            lines_added,
            lines_deleted,
            subsystem_count: subsystems.len(),
            security_sensitive_paths: security,
            dependency_changes: deps,
            test_files_changed: tests,
            generated_or_lockfile_changes: generated,
            binary_changes: binary,
            governance_paths: governance,
            validation_count: context.validation_count,
            unresolved_concerns: context.unresolved_concerns,
            repair_iterations: context.repair_iterations,
            summary_lines,
            surface_fingerprint,
        }
    }

    /// Render the compact review-budget block shown to reviewers.
    pub fn render(&self) -> String {
        let mut out = String::from("Review surface\n");
        for line in &self.summary_lines {
            out.push_str(&format!("  {line}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(path: &str, added: usize, deleted: usize) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            lines_added: added,
            lines_deleted: deleted,
            is_binary: looks_binary_extension(path),
        }
    }

    #[test]
    fn surface_counts_are_deterministic_and_classified() {
        let changes = vec![
            changed("src/core/run.rs", 100, 10),
            changed("src/auth/session.rs", 20, 5),
            changed("tests/run_test.rs", 30, 0),
            changed("Cargo.toml", 2, 1),
            changed("Cargo.lock", 40, 40),
            changed("assets/logo.png", 0, 0),
            changed(".github/workflows/ci.yml", 5, 5),
        ];
        let surface = ReviewSurface::compute(
            &changes,
            ReviewContext {
                validation_count: 3,
                unresolved_concerns: 1,
                repair_iterations: 2,
            },
        );
        assert_eq!(surface.files_changed, 7);
        assert_eq!(surface.lines_added + surface.lines_deleted, 258);
        assert_eq!(surface.security_sensitive_paths, 1);
        assert_eq!(surface.dependency_changes, 1);
        assert_eq!(surface.test_files_changed, 1);
        assert_eq!(surface.generated_or_lockfile_changes, 1);
        assert_eq!(surface.binary_changes, 1);
        assert_eq!(surface.governance_paths, 1);
        // src/core, src/auth, tests/run_test.rs→tests, Cargo.*→file-level, assets/logo, .github/workflows
        assert!(surface.subsystem_count >= 5);
        assert!(surface.render().contains("Review surface"));
    }

    #[test]
    fn fingerprint_changes_with_any_change() {
        let s1 = ReviewSurface::compute(&[changed("a.rs", 1, 0)], ReviewContext::default());
        let s2 = ReviewSurface::compute(&[changed("a.rs", 2, 0)], ReviewContext::default());
        assert_ne!(s1.surface_fingerprint, s2.surface_fingerprint);
    }

    #[test]
    fn security_heuristic_catches_auth_and_crypto_paths() {
        for path in [
            "src/auth/oauth.rs",
            "lib/crypto/sign.js",
            "internal/password_reset.py",
            "pkg/acl/manager.go",
        ] {
            assert!(is_security_sensitive(path), "path: {path}");
        }
        for path in ["src/ui/button.rs", "docs/guide.md"] {
            assert!(!is_security_sensitive(path), "path: {path}");
        }
    }

    #[test]
    fn governance_heuristic_catches_policy_paths() {
        for path in [
            ".github/contribai.yml",
            ".github/workflows/release.yml",
            "SECURITY.md",
            "CONTRIBUTING.md",
            ".devin/rules.md",
            "CODEOWNERS",
        ] {
            assert!(is_governance_adjacent(path), "path: {path}");
        }
        assert!(!is_governance_adjacent("src/main.rs"));
    }

    #[test]
    fn multiset_diff_is_exact_for_added_and_deleted_files() {
        let (added, deleted) = line_diff_counts("", "a\nb\nc\n");
        assert_eq!((added, deleted), (3, 0));
        let (added, deleted) = line_diff_counts("a\nb\n", "");
        assert_eq!((added, deleted), (0, 2));
        // Modification: shared lines don't count twice.
        let (added, deleted) = line_diff_counts("keep\nold\n", "keep\nnew\n");
        assert_eq!((added, deleted), (1, 1));
    }

    #[test]
    fn file_change_conversion_normalizes_separators() {
        let change = FileChange {
            path: "src\\core\\run.rs".into(),
            original_content: Some("a\n".into()),
            new_content: "a\nb\n".into(),
            is_new_file: false,
            is_deleted: false,
        };
        let cf = ChangedFile::from_file_change(&change);
        assert_eq!(cf.path, "src/core/run.rs");
        assert_eq!((cf.lines_added, cf.lines_deleted), (1, 0));
    }
}
