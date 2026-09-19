//! Deterministic validation graph.
//!
//! Validation is an explicit graph of evidence, not a model judgment. Each
//! check records its mechanism (argv command or static rule), timestamps,
//! result, a bounded output digest, and whether it was required. A passing
//! LLM critique can never override a failing required deterministic check.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How a check produces evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckMechanism {
    /// An argv command executed in the run workspace.
    Command { argv: Vec<String> },
    /// A deterministic static rule evaluated in-process.
    Static { rule: String },
    /// A check that could not run; `reason` explains why.
    NotRun { reason: String },
}

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail,
    /// Skipped checks are never reported as passes. `reason` is mandatory.
    Skipped,
}

impl CheckResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// Canonical check-name form: lowercase, spaces/`:` collapsed to `_`.
///
/// Manifest `required_checks` entries (`cargo_test`) and adapter display
/// names (`cargo test`) both resolve to this form, so a maintainer-named
/// required check always matches the graph node that ran it.
pub fn canonical_check_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' || c == ':' { '_' } else { c })
        .collect()
}

/// One node in the validation graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationCheck {
    /// Stable check name, e.g. `cargo_test`, `npm_lint`, `scope_policy`.
    /// Canonicalized at construction — see [`canonical_check_name`].
    pub name: String,
    /// Class grouping, e.g. `build`, `test`, `lint`, `typecheck`, `policy`,
    /// `reproduction`, `security`.
    pub category: String,
    pub mechanism: CheckMechanism,
    /// Required checks gate admission; optional checks are informational.
    pub required: bool,
    pub result: CheckResult,
    /// Mandatory when `result == Skipped`.
    pub skip_reason: Option<String>,
    /// Bounded summary — never unbounded logs.
    pub summary: String,
    /// SHA-256 over the bounded captured output, for tamper-evident linkage.
    pub output_digest: Option<String>,
    /// First N bytes of relevant output, bounded.
    pub output_excerpt: Option<String>,
    pub exit_status: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

impl ValidationCheck {
    pub fn new(
        name: impl Into<String>,
        category: impl Into<String>,
        mechanism: CheckMechanism,
        required: bool,
    ) -> Self {
        let now = Utc::now();
        Self {
            name: canonical_check_name(&name.into()),
            category: category.into(),
            mechanism,
            required,
            result: CheckResult::Skipped,
            skip_reason: Some("not executed".into()),
            summary: String::new(),
            output_digest: None,
            output_excerpt: None,
            exit_status: None,
            started_at: now,
            finished_at: now,
        }
    }

    /// Record a pass/fail with bounded evidence.
    pub fn finish(
        &mut self,
        result: CheckResult,
        summary: impl Into<String>,
        output_excerpt: Option<String>,
        exit_status: Option<i32>,
    ) {
        self.result = result;
        self.summary = crate::core::safe_truncate(&summary.into(), 400).to_string();
        self.output_excerpt = output_excerpt
            .as_deref()
            .map(|excerpt| crate::core::safe_truncate(excerpt, 2000).to_string());
        self.output_digest = output_excerpt
            .as_deref()
            .map(|excerpt| hex::encode(Sha256::digest(excerpt.as_bytes())));
        self.exit_status = exit_status;
        self.finished_at = Utc::now();
        self.skip_reason = None;
    }

    /// Record a skipped check. A skip is not a pass.
    pub fn skip(&mut self, reason: impl Into<String>) {
        self.result = CheckResult::Skipped;
        self.skip_reason = Some(crate::core::safe_truncate(&reason.into(), 200).to_string());
        self.finished_at = Utc::now();
    }
}

/// Ordered collection of validation evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationGraph {
    pub checks: Vec<ValidationCheck>,
}

/// Aggregate verdict over the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphVerdict {
    /// Every required check passed.
    Pass,
    /// At least one required check failed.
    Fail,
    /// No required check failed, but at least one required check was
    /// skipped — evidence is incomplete and admission must not proceed.
    Incomplete,
    /// The graph contains no checks at all — no evidence exists.
    Empty,
}

impl GraphVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Incomplete => "incomplete",
            Self::Empty => "empty",
        }
    }
}

impl ValidationGraph {
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    pub fn push(&mut self, check: ValidationCheck) {
        self.checks.push(check);
    }

    pub fn get(&self, name: &str) -> Option<&ValidationCheck> {
        let canonical = canonical_check_name(name);
        self.checks.iter().find(|check| check.name == canonical)
    }

    /// The deterministic verdict. Skipped required checks do not pass.
    pub fn verdict(&self) -> GraphVerdict {
        if self.checks.is_empty() {
            return GraphVerdict::Empty;
        }
        let mut incomplete = false;
        for check in &self.checks {
            if !check.required {
                continue;
            }
            match check.result {
                CheckResult::Fail => return GraphVerdict::Fail,
                CheckResult::Skipped => incomplete = true,
                CheckResult::Pass => {}
            }
        }
        if incomplete {
            GraphVerdict::Incomplete
        } else {
            GraphVerdict::Pass
        }
    }

    /// Whether the named check exists and passed.
    pub fn passed(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|check| check.result == CheckResult::Pass)
    }

    /// Counts for compact reporting: (pass, fail, skipped).
    pub fn tallies(&self) -> (usize, usize, usize) {
        let mut tallies = (0, 0, 0);
        for check in &self.checks {
            match check.result {
                CheckResult::Pass => tallies.0 += 1,
                CheckResult::Fail => tallies.1 += 1,
                CheckResult::Skipped => tallies.2 += 1,
            }
        }
        tallies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, required: bool) -> ValidationCheck {
        ValidationCheck::new(
            name,
            "test",
            CheckMechanism::Command {
                argv: vec!["cargo".into(), "test".into()],
            },
            required,
        )
    }

    #[test]
    fn empty_graph_has_no_evidence() {
        assert_eq!(ValidationGraph::new().verdict(), GraphVerdict::Empty);
    }

    #[test]
    fn required_fail_dominates() {
        let mut graph = ValidationGraph::new();
        let mut a = check("build", true);
        a.finish(CheckResult::Pass, "ok", None, Some(0));
        let mut b = check("test", true);
        b.finish(CheckResult::Fail, "2 failed", Some("fail".into()), Some(1));
        let mut c = check("lint", false);
        c.finish(CheckResult::Pass, "ok", None, Some(0));
        graph.push(a);
        graph.push(b);
        graph.push(c);
        assert_eq!(graph.verdict(), GraphVerdict::Fail);
        assert_eq!(graph.tallies(), (2, 1, 0));
    }

    #[test]
    fn skipped_required_check_is_incomplete_not_pass() {
        let mut graph = ValidationGraph::new();
        let mut a = check("build", true);
        a.finish(CheckResult::Pass, "ok", None, Some(0));
        let mut b = check("test", true);
        b.skip("toolchain unavailable");
        graph.push(a);
        graph.push(b);
        assert_eq!(graph.verdict(), GraphVerdict::Incomplete);
        assert!(!graph.passed("test"));
    }

    #[test]
    fn optional_skips_do_not_block_pass() {
        let mut graph = ValidationGraph::new();
        let mut a = check("test", true);
        a.finish(CheckResult::Pass, "ok", None, Some(0));
        let mut b = check("audit", false);
        b.skip("advisory only");
        graph.push(a);
        graph.push(b);
        assert_eq!(graph.verdict(), GraphVerdict::Pass);
    }

    #[test]
    fn output_is_bounded_and_digested() {
        let mut c = check("test", true);
        let big = "x".repeat(10_000);
        c.finish(CheckResult::Pass, "ok", Some(big.clone()), Some(0));
        assert!(c.output_excerpt.as_deref().unwrap().len() <= 2000);
        assert_eq!(c.output_digest.as_deref().unwrap().len(), 64);
        assert_eq!(
            c.output_digest.as_deref().unwrap(),
            hex::encode(Sha256::digest(big.as_bytes())).as_str()
        );
    }
}
