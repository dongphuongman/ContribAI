//! Human-in-the-loop review gate before PR creation.
//!
//! Port from Python `orchestrator/review_gate.py`.
//! Pauses the pipeline to show a generated contribution for human approval.

use tracing::info;

use crate::core::admission::EvidenceCapsule;
use crate::core::error::Result;
use crate::core::models::{Contribution, Finding};

// ── ReviewAction ───────────────────────────────────────────────────────────────

/// The action chosen during a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    Approve,
    Reject,
    Skip,
}

// ── ReviewDecision ─────────────────────────────────────────────────────────────

/// Result returned by the review gate.
#[derive(Debug, Clone)]
pub struct ReviewDecision {
    pub action: ReviewAction,
    pub reason: Option<String>,
    pub reviewer: String,
}

impl ReviewDecision {
    /// Construct a decision with no reason and the default "human" reviewer.
    pub fn new(action: ReviewAction) -> Self {
        Self {
            action,
            reason: None,
            reviewer: "human".to_string(),
        }
    }

    /// Builder — attach an optional reason.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn is_approved(&self) -> bool {
        self.action == ReviewAction::Approve
    }

    pub fn is_rejected(&self) -> bool {
        self.action == ReviewAction::Reject
    }

    pub fn is_skipped(&self) -> bool {
        self.action == ReviewAction::Skip
    }
}

// ── HumanReviewer ──────────────────────────────────────────────────────────────

/// Interactive review gate. Production callers cannot pre-approve it.
pub struct HumanReviewer {
    auto_approve: bool,
}

impl HumanReviewer {
    /// Construct the mandatory interactive review gate.
    pub fn interactive() -> Self {
        Self {
            auto_approve: false,
        }
    }

    #[cfg(test)]
    fn auto_approving_for_test() -> Self {
        Self { auto_approve: true }
    }

    /// Present a contribution for human review and return the decision.
    pub async fn review(
        &self,
        contribution: &Contribution,
        finding: &Finding,
        repo_name: &str,
    ) -> Result<ReviewDecision> {
        if self.auto_approve {
            info!("Auto-approving contribution: {}", contribution.title);
            return Ok(ReviewDecision::new(ReviewAction::Approve));
        }

        display_contribution(contribution, finding, repo_name);
        prompt_decision().await
    }

    /// Review a contribution together with the independently generated evidence
    /// that will be attached to its draft pull request.
    pub async fn review_with_evidence(
        &self,
        contribution: &Contribution,
        finding: &Finding,
        repo_name: &str,
        evidence: &EvidenceCapsule,
    ) -> Result<ReviewDecision> {
        if self.auto_approve {
            info!(
                permit = %evidence.permit_id,
                "Trusted host approved contribution with evidence"
            );
            return Ok(ReviewDecision::new(ReviewAction::Approve));
        }

        display_contribution(contribution, finding, repo_name);
        println!();
        println!("  Evidence permit : {}", evidence.permit_id);
        println!("  Base revision   : {}", evidence.base_sha);
        println!(
            "  Change fingerprint: {}",
            evidence.contribution_fingerprint
        );
        println!(
            "  Review scope    : {} file(s), {} changed line(s)",
            evidence.file_count, evidence.changed_lines
        );
        println!("  Evidence expires: {}", evidence.expires_at.to_rfc3339());
        println!("  Submission      : draft PR only");
        println!("  Evidence checks :");
        for check in &evidence.checks {
            println!(
                "    [{}] {}: {}",
                if check.passed { "pass" } else { "FAIL" },
                check.name,
                check.details
            );
        }
        println!();
        prompt_decision().await
    }
}

// ── Terminal display ───────────────────────────────────────────────────────────

/// Print a formatted summary of the contribution to stdout.
fn display_contribution(contribution: &Contribution, finding: &Finding, repo_name: &str) {
    println!();
    println!(
        "==============================  Human Review Required  =============================="
    );
    println!();
    println!("  Repo       : {}", repo_name);
    println!("  Title      : {}", contribution.title);
    println!("  Type       : {}", contribution.contribution_type);
    println!("  Severity   : {}", finding.severity);
    println!("  File       : {}", finding.file_path);
    println!(
        "  Lines      : {}",
        match (finding.line_start, finding.line_end) {
            (Some(start), Some(end)) => format!("{start}-{end}"),
            (Some(start), None) => start.to_string(),
            _ => "not specified".to_string(),
        }
    );
    println!("  Commit     : {}", contribution.commit_message);
    println!(
        "  Branch     : {}",
        if contribution.branch_name.is_empty() {
            "derived deterministically from the reviewed type and finding title"
        } else {
            &contribution.branch_name
        }
    );
    println!(
        "  Changes    : {} file(s)",
        contribution.changes.len() + contribution.tests_added.len()
    );
    println!();

    println!("  Description:");
    for line in contribution.description.lines() {
        println!("    {}", line);
    }
    println!();
    println!("  Finding:");
    println!("    Title: {}", finding.title);
    for line in finding.description.lines() {
        println!("    {}", line);
    }
    if let Some(suggestion) = &finding.suggestion {
        println!("  Suggested solution:");
        for line in suggestion.lines() {
            println!("    {}", line);
        }
    }
    println!();

    // Human approval covers every byte that may be written. Do not truncate or
    // omit test changes: the evidence fingerprint binds this exact rendering.
    for (kind, change) in contribution
        .changes
        .iter()
        .map(|change| ("change", change))
        .chain(
            contribution
                .tests_added
                .iter()
                .map(|change| ("test", change)),
        )
    {
        println!(
            "  --- BEGIN {}: {} ({} bytes) ---",
            kind,
            change.path,
            change.new_content.len()
        );
        print!("{}", change.new_content);
        if !change.new_content.ends_with('\n') {
            println!();
        }
        println!("  --- END {}: {} ---", kind, change.path);
        println!();
    }

    println!("  Create this PR?  [y]es  [n]o  [s]kip");
    println!(
        "-------------------------------------------------------------------------------------"
    );
}

/// Block on stdin and return the user's decision.
async fn prompt_decision() -> Result<ReviewDecision> {
    // `tokio::task::spawn_blocking` lets us call blocking stdin reads without
    // stalling the async runtime.
    let decision = tokio::task::spawn_blocking(|| -> ReviewDecision {
        loop {
            print!("→ ");
            // Flush so the prompt appears before we block.
            use std::io::Write;
            let _ = std::io::stdout().flush();

            let mut buf = String::new();
            match std::io::stdin().read_line(&mut buf) {
                Err(_) | Ok(0) => {
                    // EOF or error → skip
                    println!("\nSkipped (interrupted)");
                    return ReviewDecision::new(ReviewAction::Skip).with_reason("interrupted");
                }
                Ok(_) => {}
            }

            match buf.trim().to_lowercase().as_str() {
                "y" | "yes" => {
                    println!("Approved — creating PR...");
                    return ReviewDecision::new(ReviewAction::Approve);
                }
                "n" | "no" => {
                    println!("Rejected — skipping this contribution");
                    return ReviewDecision::new(ReviewAction::Reject);
                }
                "s" | "skip" => {
                    println!("Skipped");
                    return ReviewDecision::new(ReviewAction::Skip);
                }
                _ => {
                    println!("Please enter y, n, or s");
                }
            }
        }
    })
    .await
    // `JoinError` means the blocking thread panicked — treat as a skip.
    .unwrap_or_else(|_| ReviewDecision::new(ReviewAction::Skip).with_reason("thread panic"));

    Ok(decision)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{Contribution, ContributionType, FileChange, Finding, Severity};
    use chrono::Utc;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    fn make_finding() -> Finding {
        Finding {
            id: "test-finding-1".to_string(),
            finding_type: ContributionType::CodeQuality,
            severity: Severity::Medium,
            title: "Test finding".to_string(),
            description: "A test finding description".to_string(),
            file_path: "src/main.rs".to_string(),
            line_start: Some(10),
            line_end: Some(20),
            suggestion: None,
            confidence: 0.9,
            priority_signals: vec![],
        }
    }

    fn make_contribution() -> Contribution {
        Contribution {
            finding: make_finding(),
            contribution_type: ContributionType::CodeQuality,
            title: "Fix code quality".to_string(),
            description: "Improves code quality in main.rs".to_string(),
            changes: vec![FileChange {
                path: "src/main.rs".to_string(),
                original_content: None,
                new_content: "fn main() {}".to_string(),
                is_new_file: false,
                is_deleted: false,
            }],
            commit_message: "fix: improve code quality".to_string(),
            tests_added: vec![],
            branch_name: "fix/code-quality".to_string(),
            generated_at: Utc::now(),
        }
    }

    // ── ReviewDecision ────────────────────────────────────────────────────────

    #[test]
    fn test_approve_helpers() {
        let d = ReviewDecision::new(ReviewAction::Approve);
        assert!(d.is_approved());
        assert!(!d.is_rejected());
        assert!(!d.is_skipped());
        assert!(d.reason.is_none());
        assert_eq!(d.reviewer, "human");
    }

    #[test]
    fn test_reject_helpers() {
        let d = ReviewDecision::new(ReviewAction::Reject);
        assert!(!d.is_approved());
        assert!(d.is_rejected());
        assert!(!d.is_skipped());
    }

    #[test]
    fn test_skip_helpers() {
        let d = ReviewDecision::new(ReviewAction::Skip);
        assert!(!d.is_approved());
        assert!(!d.is_rejected());
        assert!(d.is_skipped());
    }

    #[test]
    fn test_with_reason_builder() {
        let d = ReviewDecision::new(ReviewAction::Skip).with_reason("interrupted");
        assert_eq!(d.reason, Some("interrupted".to_string()));
        assert!(d.is_skipped());
    }

    #[test]
    fn test_with_reason_does_not_change_action() {
        let d = ReviewDecision::new(ReviewAction::Approve).with_reason("some reason");
        assert!(d.is_approved());
        assert_eq!(d.reason, Some("some reason".to_string()));
    }

    // ── HumanReviewer — test-only bypass ─────────────────────────────────────

    #[tokio::test]
    async fn test_auto_approve_returns_approve() {
        let reviewer = HumanReviewer::auto_approving_for_test();
        let contribution = make_contribution();
        let finding = make_finding();

        let decision = reviewer
            .review(&contribution, &finding, "owner/repo")
            .await
            .expect("review should succeed");

        assert!(decision.is_approved());
        assert!(!decision.is_rejected());
        assert!(!decision.is_skipped());
    }

    #[tokio::test]
    async fn test_auto_approve_does_not_prompt() {
        // When auto_approve=true the reviewer must NOT block on stdin.
        // If it did, this test would hang forever.
        let reviewer = HumanReviewer::auto_approving_for_test();
        let contribution = make_contribution();
        let finding = make_finding();

        let decision = reviewer
            .review(&contribution, &finding, "test/repo")
            .await
            .unwrap();

        assert_eq!(decision.action, ReviewAction::Approve);
    }
}
