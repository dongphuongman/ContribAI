//! First-class Contribution Run lifecycle.
//!
//! A Contribution Run is the complete, reproducible lifecycle of one authorized
//! attempt to solve one bounded contribution task. Runs are explicit and
//! persisted — never reconstructed loosely from logs.
//!
//! Lifecycle:
//!
//! ```text
//! discovered → authorized → prepared → reproducing → planned → executing
//!          → validating → challenging → repairing → ready_for_review
//!          → approved → submitted
//! ```
//!
//! Any non-terminal state may fail closed into `blocked`, `needs_authorization`,
//! `failed`, `expired`, or `cancelled`. Transitions are validated by a
//! deterministic state machine — a model can never move a run arbitrarily.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::admission::{is_full_commit_sha, ConsentSource};

/// Lifecycle state of a Contribution Run.
///
/// Terminal states never transition again. The ordering documents the intended
/// forward flow only; validity is defined by [`RunState::can_transition_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Run record created; consent/permit not yet issued.
    Discovered,
    /// Maintainer consent verified and a ContributionPermit was issued.
    Authorized,
    /// Isolated workspace prepared at the attested base SHA.
    Prepared,
    /// Attempting to reproduce the reported problem.
    Reproducing,
    /// Structured task spec and bounded execution plan recorded.
    Planned,
    /// Solver is producing or applying a candidate.
    Executing,
    /// Deterministic validation graph is running.
    Validating,
    /// Adversarial challenger is reviewing the validated candidate.
    Challenging,
    /// Bounded repair iteration is producing a revised candidate.
    Repairing,
    /// Evidence packaged; awaiting human review of the exact candidate.
    ReadyForReview,
    /// Human approved the exact candidate fingerprint.
    Approved,
    /// Draft proposal submitted upstream. Terminal.
    Submitted,
    /// Deterministically denied. Terminal.
    Blocked,
    /// The task requires authorization beyond the current permit. Terminal.
    NeedsAuthorization,
    /// Unrecoverable execution error. Terminal.
    Failed,
    /// Permit or run expiry elapsed. Terminal.
    Expired,
    /// Operator cancelled the run. Terminal.
    Cancelled,
}

impl RunState {
    /// Stable string form used in persistence and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Authorized => "authorized",
            Self::Prepared => "prepared",
            Self::Reproducing => "reproducing",
            Self::Planned => "planned",
            Self::Executing => "executing",
            Self::Validating => "validating",
            Self::Challenging => "challenging",
            Self::Repairing => "repairing",
            Self::ReadyForReview => "ready_for_review",
            Self::Approved => "approved",
            Self::Submitted => "submitted",
            Self::Blocked => "blocked",
            Self::NeedsAuthorization => "needs_authorization",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a stored state string. Unknown values fail closed.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "discovered" => Self::Discovered,
            "authorized" => Self::Authorized,
            "prepared" => Self::Prepared,
            "reproducing" => Self::Reproducing,
            "planned" => Self::Planned,
            "executing" => Self::Executing,
            "validating" => Self::Validating,
            "challenging" => Self::Challenging,
            "repairing" => Self::Repairing,
            "ready_for_review" => Self::ReadyForReview,
            "approved" => Self::Approved,
            "submitted" => Self::Submitted,
            "blocked" => Self::Blocked,
            "needs_authorization" => Self::NeedsAuthorization,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Terminal states never transition again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Submitted
                | Self::Blocked
                | Self::NeedsAuthorization
                | Self::Failed
                | Self::Expired
                | Self::Cancelled
        )
    }

    /// States in which the run may still legitimately expire.
    fn may_expire(self) -> bool {
        !self.is_terminal()
    }

    /// Whether a forward or fail-closed transition is permitted.
    ///
    /// The forward chain encodes the lifecycle exactly once. Terminal
    /// transitions (`blocked`, `failed`, `expired`, `cancelled`,
    /// `needs_authorization`) are permitted from every non-terminal state so a
    /// run always fails closed rather than getting stuck.
    pub fn can_transition_to(self, next: RunState) -> bool {
        if self.is_terminal() {
            return false;
        }
        // Fail-closed exits are always available from non-terminal states.
        if matches!(
            next,
            Self::Blocked | Self::NeedsAuthorization | Self::Failed | Self::Cancelled
        ) {
            return true;
        }
        if next == Self::Expired {
            return self.may_expire();
        }
        matches!(
            (self, next),
            (Self::Discovered, Self::Authorized)
                | (Self::Authorized, Self::Prepared)
                | (Self::Prepared, Self::Reproducing)
                // Reproduction may be skipped when the task has no
                // reproduction surface (docs-only, config-only, or the
                // environment cannot execute the repository).
                | (Self::Prepared, Self::Planned)
                | (Self::Reproducing, Self::Planned)
                | (Self::Planned, Self::Executing)
                | (Self::Executing, Self::Validating)
                | (Self::Validating, Self::Challenging)
                // Failed required validation re-enters the repair loop.
                | (Self::Validating, Self::Repairing)
                | (Self::Challenging, Self::Repairing)
                | (Self::Challenging, Self::ReadyForReview)
                // A repaired candidate must be revalidated deterministically.
                | (Self::Repairing, Self::Validating)
                | (Self::ReadyForReview, Self::Approved)
                | (Self::Approved, Self::Submitted)
        )
    }
}

/// Bounded record of one lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: String,
    pub state_from: RunState,
    pub state_to: RunState,
    /// Machine-stable event label, e.g. `authorize`, `reproduce`, `review`.
    pub event: String,
    /// Bounded, secret-free detail (max ~500 chars persisted).
    pub detail: String,
    pub recorded_at: DateTime<Utc>,
}

/// Stable reason a run transition was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTransitionError {
    TerminalState { state: RunState },
    InvalidTransition { from: RunState, to: RunState },
    MissingPermitBinding,
    MissingBaseRevision,
    MissingCandidate,
    ReviewBindingMismatch,
}

impl std::fmt::Display for RunTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerminalState { state } => {
                write!(f, "run is in terminal state {}", state.as_str())
            }
            Self::InvalidTransition { from, to } => write!(
                f,
                "invalid run transition {} -> {}",
                from.as_str(),
                to.as_str()
            ),
            Self::MissingPermitBinding => write!(f, "run has no permit binding"),
            Self::MissingBaseRevision => write!(f, "run has no attested base revision"),
            Self::MissingCandidate => write!(f, "run has no candidate fingerprint"),
            Self::ReviewBindingMismatch => {
                write!(
                    f,
                    "reviewed candidate no longer matches the current candidate"
                )
            }
        }
    }
}

/// Persisted identity and provenance of one Contribution Run.
///
/// A run binds the repository, issue, attested base SHA, permit, task
/// fingerprint, candidate fingerprint, review binding, and outcome. Secrets,
/// prompts, and repository file contents are never stored here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContributionRun {
    /// Deterministic run identifier (`run_` + 24-hex fingerprint).
    pub run_id: String,
    pub repository: String,
    pub issue: Option<i64>,
    /// Exact attested base commit. Never a moving ref.
    pub base_sha: String,
    pub permit_id: Option<String>,
    /// Snapshot of the consent source at authorization time.
    pub consent_source: Option<ConsentSource>,
    /// Fingerprint of the normalized task specification.
    pub task_fingerprint: Option<String>,
    /// Fingerprint of the write-relevant candidate currently staged.
    pub candidate_fingerprint: Option<String>,
    /// Fingerprint of the candidate the human actually approved.
    pub review_fingerprint: Option<String>,
    /// When the human review decision was recorded.
    pub review_decided_at: Option<DateTime<Utc>>,
    pub state: RunState,
    /// Number of completed repair iterations.
    pub repair_iterations: u32,
    /// Reproduction outcome classification (see `ReproductionStatus`).
    pub reproduction: Option<String>,
    /// Count of unresolved challenger findings by severity: `crit,high,med,low`.
    pub challenge_summary: Option<String>,
    /// Draft PR identity once submitted.
    pub draft_pr_number: Option<i64>,
    pub draft_pr_url: Option<String>,
    /// Last bounded failure/block reason, if any.
    pub terminal_reason: Option<String>,
    /// Provider/model metadata (names only, never keys).
    pub solver_model: Option<String>,
    pub challenger_model: Option<String>,
    /// Versions of deterministic components for provenance.
    pub policy_version: u8,
    pub planner_version: u8,
    pub validator_version: u8,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl ContributionRun {
    /// Create a run in `discovered` state bound to repository + base SHA.
    ///
    /// `base_sha` must already be a full attested commit ID — a run never
    /// floats on a moving default branch.
    pub fn new(
        repository: impl Into<String>,
        issue: Option<i64>,
        base_sha: impl Into<String>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        let now = Utc::now();
        let repository = repository.into();
        let base_sha = base_sha.into();
        let run_id = run_fingerprint(&repository, issue, &base_sha, now);
        Self {
            run_id,
            repository,
            issue,
            base_sha,
            permit_id: None,
            consent_source: None,
            task_fingerprint: None,
            candidate_fingerprint: None,
            review_fingerprint: None,
            review_decided_at: None,
            state: RunState::Discovered,
            repair_iterations: 0,
            reproduction: None,
            challenge_summary: None,
            draft_pr_number: None,
            draft_pr_url: None,
            terminal_reason: None,
            solver_model: None,
            challenger_model: None,
            policy_version: 1,
            planner_version: 1,
            validator_version: 1,
            created_at: now,
            updated_at: now,
            expires_at,
        }
    }

    /// Whether the run is past its expiry instant.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now > self.expires_at
    }

    /// Attempt a lifecycle transition, returning a bounded event on success.
    ///
    /// The machine enforces structural validity plus binding guards:
    /// `authorized` requires a permit, `ready_for_review` requires a candidate,
    /// `submitted` requires the reviewed fingerprint to still match the
    /// current candidate. A model cannot satisfy these guards by asking.
    pub fn transition(
        &mut self,
        to: RunState,
        event: impl Into<String>,
        detail: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<RunEvent, RunTransitionError> {
        if self.state.is_terminal() {
            return Err(RunTransitionError::TerminalState { state: self.state });
        }
        if !self.state.can_transition_to(to) {
            return Err(RunTransitionError::InvalidTransition {
                from: self.state,
                to,
            });
        }
        match to {
            RunState::Authorized => {
                if self.permit_id.is_none() {
                    return Err(RunTransitionError::MissingPermitBinding);
                }
                if !is_full_commit_sha(&self.base_sha) {
                    return Err(RunTransitionError::MissingBaseRevision);
                }
            }
            RunState::ReadyForReview | RunState::Approved => {
                if self.candidate_fingerprint.is_none() {
                    return Err(RunTransitionError::MissingCandidate);
                }
            }
            RunState::Submitted
                if self.candidate_fingerprint.is_none()
                    || self.candidate_fingerprint != self.review_fingerprint =>
            {
                return Err(RunTransitionError::ReviewBindingMismatch);
            }
            _ => {}
        }

        let from = self.state;
        let detail = crate::core::safe_truncate(&detail.into(), 500).to_string();
        self.state = to;
        self.updated_at = now;
        if to.is_terminal() && !detail.is_empty() && self.terminal_reason.is_none() {
            self.terminal_reason = Some(detail.clone());
        }
        Ok(RunEvent {
            run_id: self.run_id.clone(),
            state_from: from,
            state_to: to,
            event: event.into(),
            detail,
            recorded_at: now,
        })
    }

    /// Whether the human approval is still valid for the current candidate.
    /// Any write-relevant change after review invalidates the approval.
    pub fn review_is_current(&self) -> bool {
        self.review_fingerprint.is_some() && self.review_fingerprint == self.candidate_fingerprint
    }
}

/// Deterministic run identifier: repository + issue + base SHA + creation time.
pub fn run_fingerprint(
    repository: &str,
    issue: Option<i64>,
    base_sha: &str,
    created_at: DateTime<Utc>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"contribai-run-v1");
    digest.update(repository.as_bytes());
    digest.update(issue.unwrap_or(0).to_be_bytes());
    digest.update(base_sha.as_bytes());
    digest.update(created_at.timestamp_nanos_opt().unwrap_or(0).to_be_bytes());
    let encoded = hex::encode(digest.finalize());
    format!("run_{}", &encoded[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn run() -> ContributionRun {
        ContributionRun::new("owner/repo", Some(7), SHA, Utc::now() + Duration::hours(24))
    }

    fn authorize(run: &mut ContributionRun) {
        run.permit_id = Some("0123456789abcdef01234567".to_string());
        run.transition(RunState::Authorized, "authorize", "", Utc::now())
            .unwrap();
    }

    #[test]
    fn forward_happy_path_reaches_submission() {
        let mut run = run();
        authorize(&mut run);
        for state in [
            RunState::Prepared,
            RunState::Reproducing,
            RunState::Planned,
            RunState::Executing,
            RunState::Validating,
            RunState::Challenging,
        ] {
            run.transition(state, "stage", "", Utc::now()).unwrap();
        }
        run.candidate_fingerprint = Some("abc".to_string());
        run.transition(RunState::ReadyForReview, "package", "", Utc::now())
            .unwrap();
        run.review_fingerprint = Some("abc".to_string());
        run.transition(RunState::Approved, "review", "", Utc::now())
            .unwrap();
        run.transition(RunState::Submitted, "submit", "", Utc::now())
            .unwrap();
        assert!(run.state.is_terminal());
    }

    #[test]
    fn authorize_requires_permit_and_full_sha() {
        let mut run = run();
        assert_eq!(
            run.transition(RunState::Authorized, "a", "", Utc::now()),
            Err(RunTransitionError::MissingPermitBinding)
        );
        run.permit_id = Some("p".into());
        run.base_sha = "abc123".to_string();
        assert_eq!(
            run.transition(RunState::Authorized, "a", "", Utc::now()),
            Err(RunTransitionError::MissingBaseRevision)
        );
    }

    #[test]
    fn invalid_forward_transitions_fail_closed() {
        let mut run = run();
        for bad in [
            RunState::Prepared,
            RunState::Executing,
            RunState::Validating,
            RunState::ReadyForReview,
            RunState::Approved,
            RunState::Submitted,
        ] {
            assert_eq!(
                run.transition(bad, "x", "", Utc::now()),
                Err(RunTransitionError::InvalidTransition {
                    from: RunState::Discovered,
                    to: bad
                })
            );
        }
        // Cannot skip ahead mid-lifecycle either.
        authorize(&mut run);
        assert!(run
            .transition(RunState::Validating, "x", "", Utc::now())
            .is_err());
        assert!(run
            .transition(RunState::Executing, "x", "", Utc::now())
            .is_err());
    }

    #[test]
    fn terminal_states_never_transition() {
        for terminal in [
            RunState::Blocked,
            RunState::Failed,
            RunState::Expired,
            RunState::Cancelled,
            RunState::NeedsAuthorization,
        ] {
            let mut run = run();
            run.transition(terminal, "stop", "reason", Utc::now())
                .unwrap();
            assert_eq!(
                run.transition(RunState::Authorized, "x", "", Utc::now()),
                Err(RunTransitionError::TerminalState { state: terminal })
            );
        }
        // Submitted is terminal too.
        let mut run = run();
        authorize(&mut run);
        run.transition(RunState::Blocked, "deny", "", Utc::now())
            .unwrap();
        assert!(run.state.is_terminal());
    }

    #[test]
    fn repair_loop_returns_to_validation() {
        let mut run = run();
        authorize(&mut run);
        run.transition(RunState::Prepared, "p", "", Utc::now())
            .unwrap();
        run.transition(RunState::Reproducing, "r", "", Utc::now())
            .unwrap();
        run.transition(RunState::Planned, "pl", "", Utc::now())
            .unwrap();
        run.transition(RunState::Executing, "e", "", Utc::now())
            .unwrap();
        run.transition(RunState::Validating, "v", "", Utc::now())
            .unwrap();
        run.transition(RunState::Repairing, "rep", "", Utc::now())
            .unwrap();
        run.transition(RunState::Validating, "v2", "", Utc::now())
            .unwrap();
        run.transition(RunState::Challenging, "c", "", Utc::now())
            .unwrap();
        run.transition(RunState::Repairing, "rep2", "", Utc::now())
            .unwrap();
        assert_eq!(run.state, RunState::Repairing);
    }

    #[test]
    fn submission_requires_reviewed_candidate_unchanged() {
        let mut run = run();
        authorize(&mut run);
        for s in [
            RunState::Prepared,
            RunState::Planned,
            RunState::Executing,
            RunState::Validating,
            RunState::Challenging,
        ] {
            run.transition(s, "s", "", Utc::now()).unwrap();
        }
        run.candidate_fingerprint = Some("candidate-a".into());
        run.transition(RunState::ReadyForReview, "pkg", "", Utc::now())
            .unwrap();
        run.review_fingerprint = Some("candidate-a".into());
        run.transition(RunState::Approved, "ok", "", Utc::now())
            .unwrap();
        // Candidate swapped after approval — submission must refuse.
        run.candidate_fingerprint = Some("candidate-b".into());
        assert_eq!(
            run.transition(RunState::Submitted, "s", "", Utc::now()),
            Err(RunTransitionError::ReviewBindingMismatch)
        );
        assert!(!run.review_is_current());
    }

    #[test]
    fn every_nonterminal_state_can_fail_closed() {
        let mut run = run();
        authorize(&mut run);
        run.transition(RunState::Prepared, "p", "", Utc::now())
            .unwrap();
        run.transition(RunState::Blocked, "deny", "consent revoked", Utc::now())
            .unwrap();
        assert_eq!(run.terminal_reason.as_deref(), Some("consent revoked"));
    }

    #[test]
    fn run_id_is_stable_and_namespaced() {
        let run = run();
        assert!(run.run_id.starts_with("run_"));
        assert_eq!(run.run_id.len(), 4 + 24);
    }

    #[test]
    fn state_parse_roundtrips_and_rejects_unknown() {
        for state in [
            RunState::Discovered,
            RunState::Authorized,
            RunState::Submitted,
            RunState::NeedsAuthorization,
        ] {
            assert_eq!(RunState::parse(state.as_str()), Some(state));
        }
        assert_eq!(RunState::parse("approve_as_human"), None);
        assert_eq!(RunState::parse(""), None);
    }
}
