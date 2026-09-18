//! Adversarial challenge layer.
//!
//! The solver must never review itself. After deterministic validation, a
//! logically separate Challenger inspects the candidate diff, task spec, and
//! validation evidence, producing structured findings. Findings are recorded
//! and surfaced — the Challenger never determines final admission.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a challenger finding attacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeCategory {
    /// Requirement misunderstood or only partially satisfied.
    MisunderstoodRequirement,
    /// Behavior regressed relative to the base revision.
    Regression,
    /// Edge case not covered by the candidate.
    MissingEdgeCase,
    /// Tests exist but would pass while behavior stays wrong.
    WeakTests,
    /// Security-relevant defect (injection, auth, secret, unsafe IO).
    Security,
    /// Public API or behavior compatibility broken.
    Compatibility,
    /// Candidate exceeds the authorized task scope.
    ScopeExpansion,
    /// Hidden dependency or manifest modification.
    DependencyChange,
    /// Implementation likely to break under realistic variation.
    Brittleness,
    /// Generated-code artifact (dead code, TODO, placeholder, AI smell).
    GeneratedArtifact,
    /// Governance/policy/automation file touched.
    GovernanceViolation,
    /// Anything else worth reviewer attention.
    Other,
}

impl ChallengeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MisunderstoodRequirement => "misunderstood_requirement",
            Self::Regression => "regression",
            Self::MissingEdgeCase => "missing_edge_case",
            Self::WeakTests => "weak_tests",
            Self::Security => "security",
            Self::Compatibility => "compatibility",
            Self::ScopeExpansion => "scope_expansion",
            Self::DependencyChange => "dependency_change",
            Self::Brittleness => "brittleness",
            Self::GeneratedArtifact => "generated_artifact",
            Self::GovernanceViolation => "governance_violation",
            Self::Other => "other",
        }
    }

    /// Parse a model-produced category label into a known category.
    /// Unknown labels degrade to `Other` — never silently dropped.
    pub fn parse(label: &str) -> Self {
        let normalized = label.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "misunderstood_requirement" | "requirements" | "wrong_requirement" => {
                Self::MisunderstoodRequirement
            }
            "regression" => Self::Regression,
            "missing_edge_case" | "edge_case" | "edge" => Self::MissingEdgeCase,
            "weak_tests" | "incomplete_tests" | "test_gap" => Self::WeakTests,
            "security" | "security_issue" | "vulnerability" => Self::Security,
            "compatibility" | "breaking" | "breaking_change" | "api_break" => Self::Compatibility,
            "scope_expansion" | "scope" | "out_of_scope" => Self::ScopeExpansion,
            "dependency_change" | "dependency" | "deps" => Self::DependencyChange,
            "brittleness" | "brittle" | "fragile" => Self::Brittleness,
            "generated_artifact" | "artifact" | "ai_artifact" | "dead_code" => {
                Self::GeneratedArtifact
            }
            "governance_violation" | "governance" | "protected_path" => Self::GovernanceViolation,
            _ => Self::Other,
        }
    }
}

/// Severity of a challenger finding (reuses the core severity scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChallengeSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl ChallengeSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn parse(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" => Self::Critical,
            "high" => Self::High,
            "medium" | "med" => Self::Medium,
            _ => Self::Low,
        }
    }

    /// Whether findings at this level count as blocking-review concerns.
    pub fn is_concern(self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }
}

/// One adversarial finding against the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeFinding {
    pub severity: ChallengeSeverity,
    pub category: ChallengeCategory,
    /// Bounded one-paragraph description.
    pub summary: String,
    /// Affected path when identifiable.
    pub file_path: Option<String>,
    /// Whether a later repair iteration addressed this finding.
    #[serde(default)]
    pub resolved: bool,
}

/// Complete challenger output for one candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChallengeReport {
    /// Whether the challenger actually ran to completion. A report that
    /// never executed is not a clean bill of health.
    pub completed: bool,
    /// Provider/model label (name only, never keys).
    pub challenger_model: String,
    pub findings: Vec<ChallengeFinding>,
    /// Fingerprint of the candidate that was challenged.
    pub candidate_fingerprint: String,
    /// Digest over normalized findings for tamper-evident storage.
    pub report_digest: String,
    pub created_at: DateTime<Utc>,
}

impl ChallengeReport {
    /// A report for a challenger that never ran — explicitly not a pass.
    pub fn not_run(candidate_fingerprint: &str, model: &str) -> Self {
        Self {
            completed: false,
            challenger_model: model.to_string(),
            findings: Vec::new(),
            candidate_fingerprint: candidate_fingerprint.to_string(),
            report_digest: String::new(),
            created_at: Utc::now(),
        }
    }

    /// Seal a completed report with its digest.
    pub fn completed(
        findings: Vec<ChallengeFinding>,
        candidate_fingerprint: &str,
        model: &str,
    ) -> Self {
        let mut report = Self {
            completed: true,
            challenger_model: model.to_string(),
            findings,
            candidate_fingerprint: candidate_fingerprint.to_string(),
            report_digest: String::new(),
            created_at: Utc::now(),
        };
        report.report_digest = report.compute_digest();
        report
    }

    pub fn compute_digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"contribai-challenge-v1");
        digest.update(self.candidate_fingerprint.as_bytes());
        digest.update(self.challenger_model.as_bytes());
        for finding in &self.findings {
            digest.update(finding.severity.as_str().as_bytes());
            digest.update(finding.category.as_str().as_bytes());
            digest.update(finding.summary.as_bytes());
            digest.update(finding.resolved.to_string().as_bytes());
            if let Some(path) = &finding.file_path {
                digest.update(path.as_bytes());
            }
        }
        hex::encode(digest.finalize())
    }

    /// Unresolved high/critical findings — the concerns a reviewer must see.
    pub fn unresolved_concerns(&self) -> Vec<&ChallengeFinding> {
        self.findings
            .iter()
            .filter(|f| !f.resolved && f.severity.is_concern())
            .collect()
    }

    /// Compact severity counts `c/h/m/l` for capsule storage.
    pub fn summary_string(&self) -> String {
        let mut counts = [0usize; 4];
        for finding in self.findings.iter().filter(|f| !f.resolved) {
            match finding.severity {
                ChallengeSeverity::Critical => counts[0] += 1,
                ChallengeSeverity::High => counts[1] += 1,
                ChallengeSeverity::Medium => counts[2] += 1,
                ChallengeSeverity::Low => counts[3] += 1,
            }
        }
        if !self.completed {
            return "challenger_not_run".to_string();
        }
        format!(
            "{} critical / {} high / {} medium / {} low unresolved",
            counts[0], counts[1], counts[2], counts[3]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: ChallengeSeverity, cat: ChallengeCategory, resolved: bool) -> ChallengeFinding {
        ChallengeFinding {
            severity: sev,
            category: cat,
            summary: format!("{cat:?} issue"),
            file_path: Some("src/lib.rs".into()),
            resolved,
        }
    }

    #[test]
    fn category_parsing_never_drops_findings() {
        for (input, expected) in [
            ("security", ChallengeCategory::Security),
            ("SCOPE-EXPANSION", ChallengeCategory::ScopeExpansion),
            ("edge case", ChallengeCategory::Other),
            ("edge_case", ChallengeCategory::MissingEdgeCase),
            ("gibberish", ChallengeCategory::Other),
            ("governance", ChallengeCategory::GovernanceViolation),
        ] {
            assert_eq!(ChallengeCategory::parse(input), expected, "input: {input}");
        }
    }

    #[test]
    fn unresolved_concerns_counts_only_high_and_critical() {
        let report = ChallengeReport::completed(
            vec![
                finding(
                    ChallengeSeverity::Critical,
                    ChallengeCategory::Security,
                    false,
                ),
                finding(
                    ChallengeSeverity::High,
                    ChallengeCategory::Regression,
                    false,
                ),
                finding(
                    ChallengeSeverity::Medium,
                    ChallengeCategory::WeakTests,
                    false,
                ),
                finding(
                    ChallengeSeverity::High,
                    ChallengeCategory::ScopeExpansion,
                    true,
                ),
            ],
            "fp",
            "challenger-model",
        );
        assert_eq!(report.unresolved_concerns().len(), 2);
        assert_eq!(
            report.summary_string(),
            "1 critical / 1 high / 1 medium / 0 low unresolved"
        );
    }

    #[test]
    fn not_run_report_is_explicitly_not_a_pass() {
        let report = ChallengeReport::not_run("fp", "none");
        assert!(!report.completed);
        assert_eq!(report.summary_string(), "challenger_not_run");
    }

    #[test]
    fn digest_covers_findings_and_resolution() {
        let mut a = ChallengeReport::completed(
            vec![finding(
                ChallengeSeverity::High,
                ChallengeCategory::Security,
                false,
            )],
            "fp",
            "m",
        );
        let mut b = a.clone();
        b.findings[0].resolved = true;
        b.report_digest = b.compute_digest();
        assert_ne!(a.report_digest, b.report_digest);
        a.report_digest = a.compute_digest();
        assert_eq!(a.report_digest.len(), 64);
    }
}
