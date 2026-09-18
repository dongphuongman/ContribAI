//! Evidence capsule schema 3 — bound to a Contribution Run.
//!
//! A v3 capsule carries everything a maintainer needs to verify a proposal
//! cheaply: which run produced it, which task spec it solved, whether the
//! bug reproduced, what deterministic checks ran, what the adversarial
//! challenger found, how many repair rounds occurred, and — critically —
//! the fingerprint of the exact candidate the human approved.
//!
//! `validate_for_submission` recomputes every locally checkable claim at the
//! write boundary: the capsule cannot drift from the run, the candidate, or
//! the reviewed fingerprint. A substituted candidate fails closed.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::admission::{
    changed_line_count, contribution_fingerprint, is_full_commit_sha, repository_path_error,
    AdmissionReport, ConsentSource, ContributionPermit, EvidenceCheck, EvidenceViolation,
    CONSENT_PATHS, MAINTAINER_APPROVAL_LABELS,
};
use super::challenge::ChallengeReport;
use super::models::{Contribution, Repository};
use super::review_surface::ReviewSurface;
use super::run::{ContributionRun, RunState};
use super::task_spec::TaskSpec;
use super::validation_graph::{CheckResult, GraphVerdict, ValidationGraph};

/// Evidence capsule schema implemented by this module.
pub const EVIDENCE_SCHEMA_VERSION_V3: u8 = 3;

/// Reproduction evidence recorded for the capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproductionEvidence {
    /// Whether a reproduction attempt actually ran.
    pub attempted: bool,
    /// Whether the reported behavior was reproduced at the base revision.
    pub reproduced: bool,
    /// Bounded description of the mechanism, e.g. `command: cargo test`.
    pub mechanism: String,
    /// Digest over the bounded captured output.
    pub output_digest: Option<String>,
}

/// Challenger evidence in compact form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeEvidence {
    /// Whether the challenger ran to completion. `false` is not a pass.
    pub completed: bool,
    pub challenger_model: String,
    /// Tamper-evident digest over normalized findings.
    pub report_digest: String,
    /// Unresolved findings by severity.
    pub unresolved_critical: u32,
    pub unresolved_high: u32,
    pub unresolved_medium: u32,
    pub unresolved_low: u32,
}

impl ChallengeEvidence {
    /// High/critical unresolved concerns block admission.
    pub fn has_blocking_concerns(&self) -> bool {
        !self.completed || self.unresolved_critical > 0 || self.unresolved_high > 0
    }
}

/// Evidence capsule bound to a [`ContributionRun`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceCapsuleV3 {
    pub schema_version: u8,
    /// Run this evidence belongs to. Verified against the live run record.
    pub run_id: String,
    pub permit_id: String,
    pub repository: String,
    pub base_sha: String,
    /// Fingerprint of the candidate this capsule describes.
    pub contribution_fingerprint: String,
    /// Fingerprint of the normalized task spec the candidate solved.
    pub task_fingerprint: String,
    /// Digest over the maintainer-authored inputs the spec was built from.
    pub task_source_digest: String,
    pub consent: ConsentSource,
    pub issue: Option<i64>,
    /// Reproduction evidence; `None` when the stage was not required and
    /// not attempted.
    pub reproduction: Option<ReproductionEvidence>,
    /// Validation graph flattened into checkable claims plus the policy
    /// check inserted first.
    pub checks: Vec<EvidenceCheck>,
    /// Aggregate validation verdict: `pass`, `fail`, `incomplete`, `empty`.
    pub validation_verdict: String,
    /// Challenger evidence; `None` only when the run never reached the
    /// challenge stage — which fails submission validation.
    pub challenge: Option<ChallengeEvidence>,
    pub repair_iterations: u32,
    /// Deterministic review-cost surface for the human gate.
    pub review_surface_fingerprint: Option<String>,
    /// Compact deterministic summary lines rendered for the reviewer.
    pub review_surface_lines: Vec<String>,
    /// Fingerprint of the candidate the human approved. Set only after
    /// review; submission requires it to equal `contribution_fingerprint`.
    pub review_fingerprint: Option<String>,
    pub draft_only: bool,
    pub file_count: usize,
    pub changed_lines: usize,
    pub paths: Vec<String>,
    /// Provider/model labels (names only, never keys).
    pub solver_model: Option<String>,
    pub challenger_model: Option<String>,
    pub policy_version: u8,
    pub validator_version: u8,
    pub generated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl EvidenceCapsuleV3 {
    /// Build a capsule from run state plus its stage artifacts.
    ///
    /// All bindings are copied from the run record and the artifacts, so a
    /// capsule can never claim a run, task, or candidate it did not come
    /// from. The `review_fingerprint` is set later by the review gate.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        run: &ContributionRun,
        contribution: &Contribution,
        permit: &ContributionPermit,
        report: &AdmissionReport,
        task_spec: &TaskSpec,
        validation: &ValidationGraph,
        challenge: Option<&ChallengeReport>,
        reproduction: Option<ReproductionEvidence>,
        review_surface: Option<&ReviewSurface>,
    ) -> Self {
        let mut checks: Vec<EvidenceCheck> = vec![EvidenceCheck {
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
        }];
        for check in &validation.checks {
            checks.push(EvidenceCheck {
                name: check.name.clone(),
                passed: check.result == CheckResult::Pass,
                details: match check.result {
                    CheckResult::Skipped => format!(
                        "skipped: {}",
                        check.skip_reason.as_deref().unwrap_or("no reason")
                    ),
                    _ => check.summary.clone(),
                },
            });
        }

        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION_V3,
            run_id: run.run_id.clone(),
            permit_id: permit.id.clone(),
            repository: permit.repository.clone(),
            base_sha: permit.base_sha.clone(),
            contribution_fingerprint: contribution_fingerprint(contribution),
            task_fingerprint: task_spec.compute_fingerprint(),
            task_source_digest: task_spec.source_digest.clone(),
            consent: permit.source.clone(),
            issue: permit.issue,
            reproduction,
            checks,
            validation_verdict: validation.verdict().as_str().to_string(),
            challenge: challenge.map(|report| {
                let mut counts = [0u32; 4];
                for finding in report.findings.iter().filter(|f| !f.resolved) {
                    match finding.severity {
                        super::challenge::ChallengeSeverity::Critical => counts[0] += 1,
                        super::challenge::ChallengeSeverity::High => counts[1] += 1,
                        super::challenge::ChallengeSeverity::Medium => counts[2] += 1,
                        super::challenge::ChallengeSeverity::Low => counts[3] += 1,
                    }
                }
                ChallengeEvidence {
                    completed: report.completed,
                    challenger_model: report.challenger_model.clone(),
                    report_digest: report.report_digest.clone(),
                    unresolved_critical: counts[0],
                    unresolved_high: counts[1],
                    unresolved_medium: counts[2],
                    unresolved_low: counts[3],
                }
            }),
            repair_iterations: run.repair_iterations,
            review_surface_fingerprint: review_surface.map(|s| s.surface_fingerprint.clone()),
            review_surface_lines: review_surface
                .map(|s| s.summary_lines.clone())
                .unwrap_or_default(),
            review_fingerprint: run.review_fingerprint.clone(),
            draft_only: permit.draft_only,
            file_count: report.file_count,
            changed_lines: report.changed_lines,
            paths: report.paths.clone(),
            solver_model: run.solver_model.clone(),
            challenger_model: run.challenger_model.clone(),
            policy_version: run.policy_version,
            validator_version: run.validator_version,
            generated_at: Utc::now(),
            expires_at: permit.expires_at,
        }
    }

    /// Record the human's approval of the exact candidate fingerprint.
    /// Called by the review gate; verified again at the write boundary.
    pub fn bind_review(&mut self, reviewed_fingerprint: &str) {
        self.review_fingerprint = Some(reviewed_fingerprint.to_string());
    }

    /// Recompute every locally checkable claim against the live run record
    /// before a GitHub write begins.
    ///
    /// Fails closed on: run binding mismatch, stale review approval,
    /// incomplete validation, unresolved challenger concerns, missing
    /// required reproduction evidence, expired window, scope drift, or any
    /// invalid path in the candidate.
    pub fn validate_for_submission(
        &self,
        contribution: &Contribution,
        repository: &Repository,
        run: &ContributionRun,
        permit: &ContributionPermit,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), Vec<EvidenceViolation>> {
        let mut violations = Vec::new();

        if self.schema_version != EVIDENCE_SCHEMA_VERSION_V3 {
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
        } else if now > self.expires_at || run.is_expired(now) {
            violations.push(EvidenceViolation::Expired);
        }
        if !self.draft_only {
            violations.push(EvidenceViolation::NotDraftOnly);
        }

        // ── Run binding ────────────────────────────────────────────────
        if self.run_id != run.run_id {
            violations.push(EvidenceViolation::RunBindingMismatch);
        }
        if run.state != RunState::Approved && run.state != RunState::Submitted {
            violations.push(EvidenceViolation::RunNotApproved);
        }
        if run.repository != self.repository || run.base_sha != self.base_sha {
            violations.push(EvidenceViolation::RunBindingMismatch);
        }
        match &run.task_fingerprint {
            Some(task) if *task == self.task_fingerprint => {}
            _ => violations.push(EvidenceViolation::TaskBindingMismatch),
        }
        match &run.candidate_fingerprint {
            Some(candidate) if *candidate == self.contribution_fingerprint => {}
            _ => violations.push(EvidenceViolation::ContributionMismatch),
        }

        // ── Review binding: the human approved exactly this candidate ──
        match (&self.review_fingerprint, &run.review_fingerprint) {
            (Some(capsule_review), Some(run_review))
                if capsule_review == run_review
                    && *run_review == self.contribution_fingerprint
                    && run.review_is_current() => {}
            _ => violations.push(EvidenceViolation::ReviewBindingMismatch),
        }

        // ── Consent source validity ─────────────────────────────────────
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

        // ── Scope recomputation ─────────────────────────────────────────
        let changes: Vec<&super::models::FileChange> = contribution
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

        // ── Validation graph verdict ────────────────────────────────────
        match self.validation_verdict.as_str() {
            "pass" => {}
            "incomplete" | "empty" => violations.push(EvidenceViolation::ValidationIncomplete),
            _ => violations.push(EvidenceViolation::ValidationFailed),
        }

        // ── Check integrity ─────────────────────────────────────────────
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

        // ── Required checks from the permit must exist and pass ─────────
        for name in &permit.required_checks {
            let passed = self
                .checks
                .iter()
                .any(|check| check.name == *name && check.passed);
            if !passed {
                violations.push(EvidenceViolation::MissingRequiredCheck { name: name.clone() });
            }
        }

        // ── Challenger ──────────────────────────────────────────────────
        match &self.challenge {
            Some(evidence) if !evidence.has_blocking_concerns() => {}
            Some(_) => violations.push(EvidenceViolation::UnresolvedChallengeConcerns),
            None => violations.push(EvidenceViolation::ChallengerNotRun),
        }

        // ── Reproduction ────────────────────────────────────────────────
        if permit.required_reproduction {
            match &self.reproduction {
                Some(repro) if repro.attempted && repro.reproduced => {}
                _ => violations.push(EvidenceViolation::ReproductionMissing),
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Compact markdown rendering for the draft PR body.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("### Contribution evidence (v3)\n\n");
        out.push_str(&format!("- run: `{}`\n", self.run_id));
        out.push_str(&format!("- permit: `{}`\n", self.permit_id));
        out.push_str(&format!("- base: `{}`\n", self.base_sha));
        out.push_str(&format!(
            "- candidate fingerprint: `{}`\n",
            self.contribution_fingerprint
        ));
        out.push_str(&format!(
            "- task fingerprint: `{}`\n",
            self.task_fingerprint
        ));
        if let Some(review) = &self.review_fingerprint {
            out.push_str(&format!("- human-approved fingerprint: `{review}`\n"));
        }
        out.push_str(&format!(
            "- validation: {} ({} checks)\n",
            self.validation_verdict,
            self.checks.len().saturating_sub(1)
        ));
        match &self.challenge {
            Some(c) => out.push_str(&format!(
                "- challenger: {} ({}c/{}h/{}m/{}l unresolved)\n",
                if c.completed { "ran" } else { "not run" },
                c.unresolved_critical,
                c.unresolved_high,
                c.unresolved_medium,
                c.unresolved_low
            )),
            None => out.push_str("- challenger: not run\n"),
        }
        if let Some(repro) = &self.reproduction {
            out.push_str(&format!(
                "- reproduction: {}\n",
                if repro.reproduced {
                    "reproduced at base"
                } else if repro.attempted {
                    "attempted, not reproduced"
                } else {
                    "not attempted"
                }
            ));
        }
        if self.repair_iterations > 0 {
            out.push_str(&format!(
                "- repair iterations: {}\n",
                self.repair_iterations
            ));
        }
        out.push_str(&format!(
            "- scope: {} files, {} changed lines\n",
            self.file_count, self.changed_lines
        ));
        out.push_str("\nChecks:\n");
        for check in &self.checks {
            out.push_str(&format!(
                "- {} `{}` — {}\n",
                if check.passed { "✅" } else { "❌" },
                check.name,
                check.details
            ));
        }
        out
    }
}

/// Whether the capsule's validation verdict permits admission at all.
pub fn verdict_allows_admission(verdict: &str) -> bool {
    verdict == GraphVerdict::Pass.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::admission::{AdmissionViolation, ConsentSource};
    use crate::core::models::{
        Contribution, ContributionType, FileChange, Finding, Repository, Severity,
    };
    use crate::core::run::ContributionRun;
    use crate::core::task_spec::{TaskInputs, TaskSpec};
    use crate::core::validation_graph::{CheckMechanism, ValidationCheck, ValidationGraph};
    use chrono::Duration;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repository() -> Repository {
        Repository {
            owner: "octo".into(),
            name: "repo".into(),
            full_name: "octo/repo".into(),
            description: None,
            language: Some("Rust".into()),
            languages: Default::default(),
            stars: 1,
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
        }
    }

    fn contribution() -> Contribution {
        Contribution {
            finding: Finding {
                id: "f1".into(),
                finding_type: ContributionType::CodeQuality,
                severity: Severity::Medium,
                title: "fix the thing".into(),
                description: "desc".into(),
                file_path: "src/lib.rs".into(),
                line_start: Some(1),
                line_end: Some(2),
                suggestion: None,
                confidence: 0.9,
                priority_signals: vec![],
            },
            contribution_type: ContributionType::CodeQuality,
            title: "fix: the thing".into(),
            description: "fixes it".into(),
            changes: vec![FileChange {
                path: "src/lib.rs".into(),
                original_content: Some("a\n".into()),
                new_content: "a\nb\n".into(),
                is_new_file: false,
                is_deleted: false,
            }],
            commit_message: "fix: the thing".into(),
            tests_added: vec![],
            branch_name: String::new(),
            generated_at: Utc::now(),
        }
    }

    fn permit(repo: &Repository) -> ContributionPermit {
        let consent = crate::core::admission::RepositoryConsent {
            source: ConsentSource::RepositoryManifest {
                path: CONSENT_PATHS[0].to_string(),
            },
            schema_version: 1,
            max_files: 10,
            max_changed_lines: 500,
            allowed_paths: vec!["src/**".into()],
            denied_paths: vec![],
            required_checks: vec![],
            allow_dependency_changes: true,
            allow_new_files: true,
            allow_test_changes: true,
            required_reproduction: false,
            execution_mode: crate::core::admission::ExecutionMode::Local,
            max_runtime_seconds: 900,
            allowed_issue_labels: vec![],
            draft_only: true,
        };
        ContributionPermit::issue(repo, SHA.to_string(), consent, Some(7))
    }

    fn task_spec() -> TaskSpec {
        let inputs = TaskInputs {
            issue_title: "fix the thing",
            issue_body: "it is broken",
            maintainer_comments: &[],
            policy_excerpt: "allowed: src/",
        };
        TaskSpec::draft("octo/repo", Some(7), "fix the thing").finalize(&inputs)
    }

    fn passing_graph() -> ValidationGraph {
        let mut check = ValidationCheck::new(
            "cargo test",
            "test",
            CheckMechanism::Command {
                argv: vec!["cargo".into(), "test".into()],
            },
            true,
        );
        check.finish(CheckResult::Pass, "42 passed", Some("ok".into()), Some(0));
        let mut graph = ValidationGraph::new();
        graph.push(check);
        graph
    }

    fn clean_challenge(candidate_fp: &str) -> ChallengeReport {
        ChallengeReport::completed(vec![], candidate_fp, "challenger-1")
    }

    fn approved_run(repo: &Repository, permit: &ContributionPermit) -> ContributionRun {
        let mut run = ContributionRun::new(
            repo.full_name.clone(),
            Some(7),
            SHA.to_string(),
            Utc::now() + Duration::hours(1),
        );
        run.permit_id = Some(permit.id.clone());
        run.consent_source = Some(permit.source.clone());
        run.task_fingerprint = Some("task-fp".into());
        run.candidate_fingerprint = Some("placeholder".into());
        run.review_fingerprint = Some("placeholder".into());
        run.state = RunState::Approved;
        run
    }

    fn capsule_fixture() -> (
        EvidenceCapsuleV3,
        Contribution,
        Repository,
        ContributionRun,
        ContributionPermit,
    ) {
        let repo = repository();
        let contribution = contribution();
        let permit = permit(&repo);
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp.clone());
        run.review_decided_at = Some(Utc::now());
        let graph = passing_graph();
        let challenge = clean_challenge(&fp);
        let mut capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &graph,
            Some(&challenge),
            None,
            None,
        );
        capsule.bind_review(&fp);
        (capsule, contribution, repo, run, permit)
    }

    #[test]
    fn well_formed_capsule_validates() {
        let (capsule, contribution, repo, run, permit) = capsule_fixture();
        assert_eq!(
            capsule.validate_for_submission(&contribution, &repo, &run, &permit, Utc::now()),
            Ok(())
        );
    }

    #[test]
    fn substituted_candidate_fails_closed() {
        let (capsule, _contribution, repo, run, permit) = capsule_fixture();
        let mut tampered = contribution();
        tampered.changes[0].new_content = "malicious\n".into();
        let violations = capsule
            .validate_for_submission(&tampered, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ContributionMismatch)));
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ScopeMismatch)));
    }

    #[test]
    fn post_review_candidate_change_breaks_review_binding() {
        let (mut capsule, contribution, repo, mut run, permit) = capsule_fixture();
        // Candidate regenerated after review — fingerprints no longer match.
        run.candidate_fingerprint = Some("different".into());
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ReviewBindingMismatch)));
        capsule.run_id = "run_ffffffffffffffffffffffff".into();
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::RunBindingMismatch)));
    }

    #[test]
    fn missing_challenger_fails_closed() {
        let repo = repository();
        let contribution = contribution();
        let permit = permit(&repo);
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp);
        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &passing_graph(),
            None,
            None,
            None,
        );
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ChallengerNotRun)));
    }

    #[test]
    fn unresolved_high_concern_blocks() {
        let repo = repository();
        let contribution = contribution();
        let permit = permit(&repo);
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp.clone());
        let challenge = ChallengeReport::completed(
            vec![crate::core::challenge::ChallengeFinding {
                severity: crate::core::challenge::ChallengeSeverity::High,
                category: crate::core::challenge::ChallengeCategory::MissingEdgeCase,
                summary: "breaks edge case".into(),
                file_path: None,
                resolved: false,
            }],
            &fp,
            "challenger-1",
        );
        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &passing_graph(),
            Some(&challenge),
            None,
            None,
        );
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::UnresolvedChallengeConcerns)));
    }

    #[test]
    fn skipped_required_check_fails_closed() {
        let repo = repository();
        let contribution = contribution();
        let permit = permit(&repo);
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp.clone());
        let mut graph = ValidationGraph::new();
        let mut check = ValidationCheck::new(
            "cargo test",
            "test",
            CheckMechanism::NotRun {
                reason: "toolchain missing".into(),
            },
            true,
        );
        check.skip("toolchain missing");
        graph.push(check);
        let challenge = clean_challenge(&fp);
        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &graph,
            Some(&challenge),
            None,
            None,
        );
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ValidationIncomplete)));
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::FailedCheck { .. })));
    }

    #[test]
    fn required_reproduction_enforced() {
        let repo = repository();
        let contribution = contribution();
        let mut permit = permit(&repo);
        permit.required_reproduction = true;
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp.clone());
        let challenge = clean_challenge(&fp);
        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &passing_graph(),
            Some(&challenge),
            Some(ReproductionEvidence {
                attempted: true,
                reproduced: false,
                mechanism: "command: cargo test".into(),
                output_digest: None,
            }),
            None,
        );
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::ReproductionMissing)));
    }

    #[test]
    fn markdown_includes_run_and_fingerprints() {
        let (capsule, _, _, _, _) = capsule_fixture();
        let md = capsule.to_markdown();
        assert!(md.contains("run:"));
        assert!(md.contains(&capsule.run_id));
        assert!(md.contains(&capsule.contribution_fingerprint));
        assert!(md.contains("human-approved fingerprint"));
    }

    #[test]
    fn non_approved_run_state_blocks() {
        let (capsule, contribution, repo, mut run, permit) = capsule_fixture();
        run.state = RunState::Executing;
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::RunNotApproved)));
    }

    #[test]
    fn stale_run_expiry_blocks() {
        let (capsule, contribution, repo, mut run, permit) = capsule_fixture();
        run.expires_at = Utc::now() - Duration::minutes(1);
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::Expired)));
    }

    #[test]
    fn missing_required_named_check_blocks() {
        let repo = repository();
        let contribution = contribution();
        let mut permit = permit(&repo);
        permit.required_checks = vec!["cargo clippy".into()];
        let report = crate::core::admission::AdmissionController::evaluate(
            &repo,
            &contribution,
            &permit,
            Utc::now(),
        );
        let spec = task_spec();
        let mut run = approved_run(&repo, &permit);
        run.task_fingerprint = Some(spec.compute_fingerprint());
        let fp = contribution_fingerprint(&contribution);
        run.candidate_fingerprint = Some(fp.clone());
        run.review_fingerprint = Some(fp.clone());
        let challenge = clean_challenge(&fp);
        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &passing_graph(),
            Some(&challenge),
            None,
            None,
        );
        let violations = capsule
            .validate_for_submission(&contribution, &repo, &run, &permit, Utc::now())
            .unwrap_err();
        assert!(violations
            .iter()
            .any(|v| matches!(v, EvidenceViolation::MissingRequiredCheck { .. })));
    }

    #[test]
    fn admission_violation_type_still_imported() {
        // Compile-time guard: shared violation type remains the same enum.
        let _ = AdmissionViolation::MissingRequiredCheck { name: "x".into() };
    }
}
