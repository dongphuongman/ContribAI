//! Contribution Run executor — drives the full v7 lifecycle.
//!
//! ```text
//! discovered → authorized → prepared → reproducing → planned → executing
//!            → validating → challenging → repairing → ready_for_review
//!            → approved → submitted
//! ```
//!
//! The executor is deterministic policy code. Everything it cannot derive
//! locally — repository reads, solver/challenger models, human review, and
//! the actual write — goes through the [`RunEnvironment`] boundary, so the
//! lifecycle is fully testable offline and no model output touches run state
//! without a deterministic gate in between.

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::core::admission::{
    evaluate_run_requirements, is_full_commit_sha, AdmissionAuditDecision, AdmissionAuditRecord,
    AdmissionAuditStage, AdmissionController, ContributionPermit, RepositoryConsent, CONSENT_PATHS,
    MAINTAINER_APPROVAL_LABELS,
};
use crate::core::challenge::{ChallengeFinding, ChallengeReport};
use crate::core::error::{ContribError, Result};
use crate::core::evidence_v3::{EvidenceCapsuleV3, ReproductionEvidence};
use crate::core::models::{
    Contribution, ContributionType, FileChange, Finding, PrResult, Repository,
};
use crate::core::review_surface::{ChangedFile, ReviewContext, ReviewSurface};
use crate::core::run::{ContributionRun, RunState};
use crate::core::task_spec::{TaskInputs, TaskSpec};
use crate::core::validation_graph::{
    CheckMechanism, CheckResult, GraphVerdict, ValidationCheck, ValidationGraph,
};
use crate::exec::adapters;
use crate::exec::runner::BoundedRunner;
use crate::exec::workspace::RunWorkspace;
use crate::orchestrator::memory::Memory;
use crate::orchestrator::review_gate::RunReviewDecision;

/// Owned maintainer-authored inputs (the borrowed form is [`TaskInputs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedTaskInputs {
    pub issue_title: String,
    pub issue_body: String,
    /// Maintainer comments only — third-party comments are untrusted.
    pub maintainer_comments: Vec<String>,
    pub policy_excerpt: String,
}

impl OwnedTaskInputs {
    pub fn borrow(&self) -> TaskInputs<'_> {
        TaskInputs {
            issue_title: &self.issue_title,
            issue_body: &self.issue_body,
            maintainer_comments: &self.maintainer_comments,
            policy_excerpt: &self.policy_excerpt,
        }
    }
}

/// Issue fields the executor needs — mirrors the GitHub issue view without
/// depending on the client type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunIssueView {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub maintainer_comments: Vec<String>,
}

/// Solver output: everything needed to build the candidate `Contribution`.
#[derive(Debug, Clone)]
pub struct SolverCandidate {
    pub title: String,
    pub description: String,
    pub commit_message: String,
    pub contribution_type: ContributionType,
    pub finding: Finding,
    pub changes: Vec<FileChange>,
    pub tests_added: Vec<FileChange>,
}

/// External world the executor cannot derive locally.
///
/// Production wires this to `GitHubClient`/`LlmProvider`/`HumanReviewer`/
/// `PrManager`; tests substitute fakes. Deterministic gates — transitions,
/// fingerprints, scope, evidence validation — never delegate to this trait.
#[async_trait]
pub trait RunEnvironment: Send + Sync {
    /// Maintainer consent manifest content at `path`, if present.
    async fn fetch_consent_manifest(&self, repo: &str, path: &str) -> Result<Option<String>>;

    /// Issue view for label consent and task text.
    async fn fetch_issue(&self, repo: &str, issue: i64) -> Result<RunIssueView>;

    /// Repository metadata for scope evaluation and submission.
    async fn fetch_repository(&self, repo: &str) -> Result<Repository>;

    /// Attested base commit — a full SHA, never a moving ref.
    async fn attest_base_sha(&self, repo: &str, issue: Option<i64>) -> Result<String>;

    /// Repository file snapshot at `base_sha` for workspace materialization.
    async fn fetch_base_snapshot(
        &self,
        repo: &str,
        base_sha: &str,
    ) -> Result<Vec<(String, Vec<u8>)>>;

    /// Maintainer-authored task inputs for the TaskSpec.
    async fn task_inputs(&self, repo: &str, issue: Option<i64>) -> Result<OwnedTaskInputs>;

    /// Reproduction commands to run at base revision. Empty = no
    /// reproduction surface (docs-only, environment cannot execute).
    async fn reproduction_commands(
        &self,
        _repo: &str,
        _spec: &TaskSpec,
    ) -> Result<Vec<Vec<String>>> {
        Ok(Vec::new())
    }

    /// Solver model produces the candidate.
    async fn solve(
        &self,
        repo: &str,
        spec: &TaskSpec,
        workspace_paths: &[String],
    ) -> Result<SolverCandidate>;

    /// Bounded repair: produce revised changes for unresolved findings.
    async fn repair(
        &self,
        repo: &str,
        spec: &TaskSpec,
        findings: &[ChallengeFinding],
        workspace_paths: &[String],
    ) -> Result<Vec<FileChange>>;

    /// Independent challenger produces adversarial findings for the
    /// candidate's diff. The executor seals the report — the model cannot
    /// mark its own output complete.
    async fn challenge(
        &self,
        repo: &str,
        spec: &TaskSpec,
        diff_summary: &str,
    ) -> Result<Vec<ChallengeFinding>>;

    /// Human review of the exact candidate + evidence.
    async fn human_review(
        &self,
        contribution: &Contribution,
        repo_name: &str,
        evidence: &EvidenceCapsuleV3,
    ) -> Result<RunReviewDecision>;

    /// Submit the draft PR via the v3 write path.
    async fn submit(
        &self,
        contribution: &Contribution,
        repo: &Repository,
        evidence: &EvidenceCapsuleV3,
        run: &ContributionRun,
        permit: &ContributionPermit,
    ) -> Result<PrResult>;

    /// Model labels for provenance (names only, never keys).
    fn solver_model_label(&self) -> Option<String> {
        None
    }
    fn challenger_model_label(&self) -> Option<String> {
        None
    }
}

/// Operator-tunable executor configuration.
#[derive(Debug, Clone)]
pub struct RunExecConfig {
    /// Root directory for isolated workspaces and artifacts.
    pub runs_root: PathBuf,
    /// Repair iterations allowed before the run blocks. Default 2.
    pub max_repair_iterations: u32,
    /// Whether approval-gated commands may run (explicit operator opt-in).
    pub allow_approval_commands: bool,
    /// The `--submit` capability grant. Without it, `submit` fails closed
    /// before any external write is attempted.
    pub submit_capable: bool,
    /// Per-command timeout for deterministic checks.
    pub command_timeout_secs: u64,
}

impl Default for RunExecConfig {
    fn default() -> Self {
        Self {
            runs_root: PathBuf::from(".contribai/runs"),
            max_repair_iterations: 2,
            allow_approval_commands: false,
            submit_capable: false,
            command_timeout_secs: 120,
        }
    }
}

/// Artifacts produced across stages, persisted as
/// `<runs_root>/<run_id>.artifacts.json` for inspectability and resume.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunArtifacts {
    /// The issued permit — reissuing would change id/expiry, so the exact
    /// issued permit is stored.
    pub permit: Option<ContributionPermit>,
    pub task_spec: Option<TaskSpec>,
    pub reproduction: Option<ReproductionEvidence>,
    pub validation: Option<ValidationGraph>,
    pub challenge: Option<ChallengeReport>,
    pub contribution: Option<Contribution>,
    pub review_surface: Option<ReviewSurface>,
    pub capsule: Option<EvidenceCapsuleV3>,
}

impl RunArtifacts {
    fn path(runs_root: &Path, run_id: &str) -> PathBuf {
        runs_root.join(format!("{run_id}.artifacts.json"))
    }

    pub fn load(runs_root: &Path, run_id: &str) -> Result<Self> {
        let path = Self::path(runs_root, run_id);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ContribError::Config(format!("artifacts read: {e}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| ContribError::Config(format!("artifacts parse: {e}")))
    }

    pub fn save(&self, runs_root: &Path, run_id: &str) -> Result<()> {
        std::fs::create_dir_all(runs_root)
            .map_err(|e| ContribError::Config(format!("artifacts dir: {e}")))?;
        let path = Self::path(runs_root, run_id);
        let tmp = runs_root.join(format!("{run_id}.artifacts.tmp"));
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| ContribError::Config(format!("artifacts encode: {e}")))?;
        std::fs::write(&tmp, raw)
            .map_err(|e| ContribError::Config(format!("artifacts write: {e}")))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| ContribError::Config(format!("artifacts rename: {e}")))?;
        Ok(())
    }
}

/// Drives one [`ContributionRun`] through the lifecycle.
pub struct RunExecutor<'a> {
    memory: &'a Memory,
    env: &'a dyn RunEnvironment,
    config: RunExecConfig,
}

impl<'a> RunExecutor<'a> {
    pub fn new(memory: &'a Memory, env: &'a dyn RunEnvironment, config: RunExecConfig) -> Self {
        Self {
            memory,
            env,
            config,
        }
    }

    fn load(&self, run_id: &str) -> Result<ContributionRun> {
        self.memory
            .get_run(run_id)?
            .ok_or_else(|| ContribError::Config(format!("run {run_id} does not exist")))
    }

    fn artifacts(&self, run_id: &str) -> Result<RunArtifacts> {
        RunArtifacts::load(&self.config.runs_root, run_id)
    }

    fn save_artifacts(&self, run_id: &str, artifacts: &RunArtifacts) -> Result<()> {
        artifacts.save(&self.config.runs_root, run_id)
    }

    /// Fail the run closed when it is terminal or expired.
    fn enforce_live(&self, run: &ContributionRun) -> Result<()> {
        if run.state.is_terminal() {
            return Err(ContribError::Config(format!(
                "run {} is terminal ({})",
                run.run_id,
                run.state.as_str()
            )));
        }
        if run.is_expired(Utc::now()) {
            self.memory.transition_run(
                &run.run_id,
                RunState::Expired,
                "expire",
                "run lifetime elapsed",
            )?;
            return Err(ContribError::Config(format!("run {} expired", run.run_id)));
        }
        Ok(())
    }

    fn terminal(
        &self,
        run_id: &str,
        to: RunState,
        event: &str,
        detail: &str,
    ) -> Result<ContributionRun> {
        Ok(self.memory.transition_run(run_id, to, event, detail)?.0)
    }

    fn runner_for(
        &self,
        workspace: &RunWorkspace,
        run: &ContributionRun,
        permit: &ContributionPermit,
    ) -> BoundedRunner {
        let remaining = permit.max_runtime_seconds.min(
            run.expires_at
                .signed_duration_since(Utc::now())
                .num_seconds()
                .max(0) as u64,
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(remaining);
        BoundedRunner::new(workspace.root())
            .with_approval_commands(self.config.allow_approval_commands)
            .with_default_timeout(std::time::Duration::from_secs(
                self.config.command_timeout_secs,
            ))
            .with_run_deadline(deadline)
    }

    // ── Stages ─────────────────────────────────────────────────────────

    /// Maintainer consent → attested base SHA → bounded permit.
    pub async fn authorize(&self, run_id: &str) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Discovered {
            return Ok(run);
        }

        // 1. Manifest consent wins over label consent: try CONSENT_PATHS.
        let mut consent: Option<RepositoryConsent> = None;
        for path in CONSENT_PATHS {
            if let Some(content) = self
                .env
                .fetch_consent_manifest(&run.repository, path)
                .await?
            {
                if let Some(parsed) = RepositoryConsent::parse(path, &content) {
                    consent = Some(parsed);
                    break;
                }
            }
        }
        // 2. Maintainer-controlled issue label as fallback.
        if consent.is_none() {
            if let Some(issue_no) = run.issue {
                let issue = self.env.fetch_issue(&run.repository, issue_no).await?;
                consent = label_consent(&issue);
            }
        }
        let Some(consent) = consent else {
            return self.terminal(
                run_id,
                RunState::NeedsAuthorization,
                "authorize",
                "no maintainer consent manifest or approval label",
            );
        };

        // 3. Attest the exact base revision — never a moving ref.
        let base_sha = self.env.attest_base_sha(&run.repository, run.issue).await?;
        if !is_full_commit_sha(&base_sha) || base_sha != run.base_sha {
            return self.terminal(
                run_id,
                RunState::Blocked,
                "authorize",
                "base revision could not be attested to the recorded full SHA",
            );
        }

        // 4. Issue the bounded permit and bind it to the run.
        let permit = ContributionPermit::issue(
            &self.env.fetch_repository(&run.repository).await?,
            base_sha,
            consent,
            run.issue,
        );
        let mut artifacts = self.artifacts(run_id)?;
        artifacts.permit = Some(permit);
        self.save_artifacts(run_id, &artifacts)?;

        run.permit_id = artifacts.permit.as_ref().map(|p| p.id.clone());
        run.consent_source = Some(artifacts.permit.as_ref().unwrap().source.clone());
        self.memory.update_run(&run)?;
        run = self
            .memory
            .transition_run(run_id, RunState::Authorized, "authorize", "permit issued")?
            .0;
        Ok(run)
    }

    /// Materialize the isolated workspace at the attested base SHA.
    pub async fn prepare(&self, run_id: &str) -> Result<RunWorkspace> {
        let run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Authorized {
            return Err(ContribError::Config(format!(
                "run {} is not authorized (state {})",
                run_id,
                run.state.as_str()
            )));
        }

        let files = self
            .env
            .fetch_base_snapshot(&run.repository, &run.base_sha)
            .await?;
        let workspace =
            RunWorkspace::materialize(&self.config.runs_root, run_id, &run.base_sha, &files)
                .await?;
        self.memory.transition_run(
            run_id,
            RunState::Prepared,
            "prepare",
            "workspace materialized",
        )?;
        Ok(workspace)
    }

    /// Build the structured task spec and bind its fingerprint to the run.
    pub async fn understand(&self, run_id: &str) -> Result<TaskSpec> {
        let run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if !matches!(run.state, RunState::Prepared | RunState::Reproducing) {
            return Err(ContribError::Config(format!(
                "run {} cannot build a task spec in state {}",
                run_id,
                run.state.as_str()
            )));
        }

        let inputs = self.env.task_inputs(&run.repository, run.issue).await?;
        let mut spec = TaskSpec::draft(&run.repository, run.issue, &inputs.issue_title);
        // Maintainer-authored requirements are separated from model
        // hypotheses — only issue text lands here.
        for line in inputs.issue_body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            spec.requirements
                .push(crate::core::task_spec::TaskRequirement {
                    provenance: crate::core::task_spec::RequirementProvenance::MaintainerAuthored,
                    text: trimmed.to_string(),
                });
        }
        if inputs.issue_body.trim().is_empty() {
            spec.ambiguities
                .push("issue body is empty — no maintainer-authored requirements".into());
        }
        let spec = spec.finalize(&inputs.borrow());

        let mut artifacts = self.artifacts(run_id)?;
        artifacts.task_spec = Some(spec.clone());
        self.save_artifacts(run_id, &artifacts)?;

        let mut run = run;
        run.task_fingerprint = Some(spec.compute_fingerprint());
        self.memory.update_run(&run)?;
        Ok(spec)
    }

    /// Attempt reproduction at the base revision, then seal the plan.
    ///
    /// When the environment exposes no reproduction surface, the run moves
    /// `prepared → planned` directly — skipping is recorded, not hidden.
    pub async fn reproduce_and_plan(&self, run_id: &str) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Prepared {
            return Err(ContribError::Config(format!(
                "run {} cannot reproduce in state {}",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let spec = artifacts.task_spec.clone().ok_or_else(|| {
            ContribError::Config("task spec missing — run understand first".into())
        })?;
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing — run authorize first".into()))?;

        let commands = self
            .env
            .reproduction_commands(&run.repository, &spec)
            .await?;

        if commands.is_empty() {
            artifacts.reproduction = Some(ReproductionEvidence {
                attempted: false,
                reproduced: false,
                mechanism: "no reproduction surface".into(),
                output_digest: None,
            });
            run.reproduction = Some("no_surface".into());
        } else {
            run = self
                .memory
                .transition_run(
                    run_id,
                    RunState::Reproducing,
                    "reproduce",
                    "attempting reproduction at base",
                )?
                .0;
            let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
            let runner = self.runner_for(&workspace, &run, &permit);

            let mut reproduced = false;
            let mut last_digest = None;
            let mut mechanism = String::new();
            for argv in &commands {
                let outcome = runner.run_indirect(argv, argv).await?;
                mechanism = format!("command: {}", argv.join(" "));
                last_digest = Some(outcome.output_digest.clone());
                // Convention: a reproduction command "reproduces" the bug
                // when it exits non-zero at base (the failing behavior is
                // demonstrably present).
                if outcome.gate == "ran" && !outcome.timed_out && !outcome.passed() {
                    reproduced = true;
                }
            }
            artifacts.reproduction = Some(ReproductionEvidence {
                attempted: true,
                reproduced,
                mechanism,
                output_digest: last_digest,
            });
            run.reproduction = Some(if reproduced {
                "reproduced".into()
            } else {
                "not_reproduced".into()
            });
        }

        if permit.required_reproduction
            && !artifacts
                .reproduction
                .as_ref()
                .map(|r| r.attempted && r.reproduced)
                .unwrap_or(false)
        {
            self.save_artifacts(run_id, &artifacts)?;
            self.memory.update_run(&run)?;
            return self.terminal(
                run_id,
                RunState::Blocked,
                "reproduce",
                "maintainer requires reproduction evidence that could not be produced",
            );
        }

        self.save_artifacts(run_id, &artifacts)?;
        self.memory.update_run(&run)?;
        run = self
            .memory
            .transition_run(
                run_id,
                RunState::Planned,
                "plan",
                "task spec and plan sealed",
            )?
            .0;
        Ok(run)
    }

    /// Solver produces the candidate inside the workspace.
    pub async fn solve(&self, run_id: &str) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Planned {
            return Err(ContribError::Config(format!(
                "run {} cannot solve in state {}",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let spec = artifacts.task_spec.clone().ok_or_else(|| {
            ContribError::Config("task spec missing — run understand first".into())
        })?;

        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        if workspace.has_unexpected_changes().await? {
            return self.terminal(
                run_id,
                RunState::Failed,
                "solve",
                "workspace has unexpected changes before solving",
            );
        }

        run = self
            .memory
            .transition_run(run_id, RunState::Executing, "solve", "solver invoked")?
            .0;

        let paths = workspace_files(&workspace);
        let candidate = self.env.solve(&run.repository, &spec, &paths).await?;

        // Apply the candidate inside the workspace through the path policy.
        for change in candidate.changes.iter().chain(candidate.tests_added.iter()) {
            if change.is_deleted {
                return self.terminal(
                    run_id,
                    RunState::Blocked,
                    "solve",
                    "solver proposed a file deletion — not supported",
                );
            }
            workspace.write_file(&change.path, change.new_content.as_bytes())?;
        }
        let mut workspace = workspace;
        workspace.checkpoint().await?;

        let contribution = Contribution {
            finding: candidate.finding,
            contribution_type: candidate.contribution_type,
            title: candidate.title,
            description: candidate.description,
            changes: candidate.changes,
            commit_message: candidate.commit_message,
            tests_added: candidate.tests_added,
            branch_name: String::new(),
            generated_at: Utc::now(),
        };
        artifacts.contribution = Some(contribution.clone());
        self.save_artifacts(run_id, &artifacts)?;

        run.candidate_fingerprint = Some(crate::core::admission::contribution_fingerprint(
            &contribution,
        ));
        run.solver_model = self.env.solver_model_label();
        self.memory.update_run(&run)?;
        Ok(run)
    }

    /// Run the deterministic validation graph in the workspace.
    pub async fn validate(&self, run_id: &str) -> Result<ValidationGraph> {
        let run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Executing && run.state != RunState::Repairing {
            return Err(ContribError::Config(format!(
                "run {} cannot validate in state {}",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing — run authorize first".into()))?;

        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        if workspace.has_unexpected_changes().await? {
            self.terminal(
                run_id,
                RunState::Failed,
                "validate",
                "workspace has unexpected changes before validation",
            )?;
            return Err(ContribError::Config("unexpected workspace changes".into()));
        }
        let runner = self.runner_for(&workspace, &run, &permit);

        self.memory.transition_run(
            run_id,
            RunState::Validating,
            "validate",
            "validation graph running",
        )?;

        let mut graph = ValidationGraph::new();
        for (ecosystem, checks) in adapters::plan_checks(workspace.root()) {
            for check in checks {
                // Named required checks from the permit override the
                // adapter's optional flag.
                let required = check.required || permit.required_checks.contains(&check.name);
                let mut node = ValidationCheck::new(
                    &check.name,
                    ecosystem.as_str(),
                    CheckMechanism::Command {
                        argv: check.argv.clone(),
                    },
                    required,
                );
                let outcome = runner.run(&check.argv).await?;
                match outcome.gate.as_str() {
                    "ran" => node.finish(
                        if outcome.passed() {
                            CheckResult::Pass
                        } else {
                            CheckResult::Fail
                        },
                        format!(
                            "exit {:?} in {}ms",
                            outcome.exit_status, outcome.duration_ms
                        ),
                        Some(format!(
                            "{}{}",
                            outcome.stdout_excerpt, outcome.stderr_excerpt
                        )),
                        outcome.exit_status,
                    ),
                    gate => node.skip(format!("command gate: {gate}")),
                }
                graph.push(node);
            }
        }
        // Permit-named checks the adapters did not cover are recorded as
        // skipped required checks — never silently absent.
        for name in &permit.required_checks {
            if graph.get(name).is_none() {
                let mut node = ValidationCheck::new(
                    name,
                    "policy",
                    CheckMechanism::NotRun {
                        reason: "no adapter provided this required check".into(),
                    },
                    true,
                );
                node.skip("no adapter provided this required check");
                graph.push(node);
            }
        }

        // Baseline required static check: scope policy re-evaluated against
        // the current candidate. Always runnable, so the graph can never be
        // empty and admission scope is verified where the candidate lives.
        if let Some(contribution) = artifacts.contribution.as_ref() {
            let repository = self.env.fetch_repository(&run.repository).await?;
            let report =
                AdmissionController::evaluate(&repository, contribution, &permit, Utc::now());
            let mut node = ValidationCheck::new(
                "admission_scope",
                "policy",
                CheckMechanism::Static {
                    rule: "scope, protected paths, budgets, consent policy".into(),
                },
                true,
            );
            node.finish(
                if report.allowed {
                    CheckResult::Pass
                } else {
                    CheckResult::Fail
                },
                if report.allowed {
                    "candidate within authorized scope".to_string()
                } else {
                    report
                        .violations
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                },
                None,
                None,
            );
            graph.push(node);
        }

        artifacts.validation = Some(graph.clone());
        self.save_artifacts(run_id, &artifacts)?;
        Ok(graph)
    }

    /// Adversarial challenger reviews the validated candidate, then the
    /// bounded repair loop decides: repair, block, or ready for review.
    pub async fn challenge_and_maybe_repair(&self, run_id: &str) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::Validating {
            return Err(ContribError::Config(format!(
                "run {} cannot challenge in state {}",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let spec = artifacts
            .task_spec
            .clone()
            .ok_or_else(|| ContribError::Config("task spec missing".into()))?;
        let graph = artifacts
            .validation
            .clone()
            .ok_or_else(|| ContribError::Config("validation graph missing".into()))?;

        // Validation verdict gates challenge entry.
        match graph.verdict() {
            GraphVerdict::Pass => {}
            GraphVerdict::Fail | GraphVerdict::Incomplete | GraphVerdict::Empty => {
                if run.repair_iterations < self.config.max_repair_iterations {
                    return self
                        .repair_once(run_id, &mut artifacts, "validation_failed")
                        .await;
                }
                return self.terminal(
                    run_id,
                    RunState::Blocked,
                    "validate",
                    "validation failed and repair budget is exhausted",
                );
            }
        }

        run = self
            .memory
            .transition_run(
                run_id,
                RunState::Challenging,
                "challenge",
                "challenger invoked",
            )?
            .0;

        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        let diff_summary = diff_summary(&workspace).await?;
        let findings = self
            .env
            .challenge(&run.repository, &spec, &diff_summary)
            .await?;
        let candidate_fp = run.candidate_fingerprint.clone().unwrap_or_default();
        let report = ChallengeReport::completed(
            findings,
            &candidate_fp,
            &self
                .env
                .challenger_model_label()
                .unwrap_or_else(|| "unknown".into()),
        );
        run.challenge_summary = Some(report.summary_string());
        run.challenger_model = Some(report.challenger_model.clone());
        let blocking = !report.unresolved_concerns().is_empty();
        artifacts.challenge = Some(report);
        self.save_artifacts(run_id, &artifacts)?;
        self.memory.update_run(&run)?;

        if blocking {
            if run.repair_iterations < self.config.max_repair_iterations {
                return self
                    .repair_once(run_id, &mut artifacts, "challenger_concerns")
                    .await;
            }
            return self.terminal(
                run_id,
                RunState::Blocked,
                "challenge",
                "unresolved challenger concerns and repair budget exhausted",
            );
        }

        run = self
            .memory
            .transition_run(
                run_id,
                RunState::ReadyForReview,
                "challenge",
                "no blocking challenger concerns",
            )?
            .0;
        Ok(run)
    }

    /// One bounded repair iteration: solver revises the candidate, the run
    /// returns to deterministic validation.
    async fn repair_once(
        &self,
        run_id: &str,
        artifacts: &mut RunArtifacts,
        trigger: &str,
    ) -> Result<ContributionRun> {
        let mut run = self
            .memory
            .transition_run(run_id, RunState::Repairing, "repair", trigger)?
            .0;

        let spec = artifacts
            .task_spec
            .clone()
            .ok_or_else(|| ContribError::Config("task spec missing".into()))?;
        let findings = artifacts
            .challenge
            .as_ref()
            .map(|c| c.findings.clone())
            .unwrap_or_default();
        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        let paths = workspace_files(&workspace);
        let revised = self
            .env
            .repair(&run.repository, &spec, &findings, &paths)
            .await?;
        for change in &revised {
            if change.is_deleted {
                return self.terminal(
                    run_id,
                    RunState::Blocked,
                    "repair",
                    "repair proposed a file deletion — not supported",
                );
            }
            workspace.write_file(&change.path, change.new_content.as_bytes())?;
        }
        let mut workspace = workspace;
        workspace.checkpoint().await?;

        // The repaired candidate replaces the staged contribution — the
        // prior human review (if any) is invalidated by the new fingerprint.
        if let Some(contribution) = artifacts.contribution.as_mut() {
            contribution.changes = revised;
            contribution.generated_at = Utc::now();
        }
        run.repair_iterations += 1;
        run.candidate_fingerprint = artifacts
            .contribution
            .as_ref()
            .map(crate::core::admission::contribution_fingerprint);
        run.review_fingerprint = None;
        run.review_decided_at = None;
        self.save_artifacts(run_id, artifacts)?;
        self.memory.update_run(&run)?;
        Ok(run)
    }

    /// Package the evidence capsule and review surface for the human gate.
    pub async fn package_evidence(&self, run_id: &str) -> Result<EvidenceCapsuleV3> {
        let run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::ReadyForReview {
            return Err(ContribError::Config(format!(
                "run {} cannot package evidence in state {}",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing".into()))?;
        let spec = artifacts
            .task_spec
            .clone()
            .ok_or_else(|| ContribError::Config("task spec missing".into()))?;
        let contribution = artifacts
            .contribution
            .clone()
            .ok_or_else(|| ContribError::Config("contribution missing".into()))?;
        let graph = artifacts.validation.clone().unwrap_or_default();

        let repository = self.env.fetch_repository(&run.repository).await?;
        let report = AdmissionController::evaluate(&repository, &contribution, &permit, Utc::now());
        if !report.allowed {
            let reason = report
                .violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return self
                .terminal(run_id, RunState::Blocked, "evidence", &reason)
                .map(|_| unreachable!());
        }
        let run_req_violations = evaluate_run_requirements(
            &permit,
            |name| graph.passed(name),
            artifacts
                .reproduction
                .as_ref()
                .map(|r| r.attempted && r.reproduced)
                .unwrap_or(false),
        );
        if !run_req_violations.is_empty() {
            let reason = run_req_violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return self
                .terminal(run_id, RunState::Blocked, "evidence", &reason)
                .map(|_| unreachable!());
        }

        // Deterministic review-cost surface from the workspace diff.
        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        let changed = workspace.changed_files().await.unwrap_or_else(|_| {
            contribution
                .changes
                .iter()
                .chain(contribution.tests_added.iter())
                .map(ChangedFile::from_file_change)
                .collect()
        });
        let unresolved = artifacts
            .challenge
            .as_ref()
            .map(|c| c.unresolved_concerns().len())
            .unwrap_or(0);
        let surface = ReviewSurface::compute(
            &changed,
            ReviewContext {
                validation_count: graph.checks.len(),
                unresolved_concerns: unresolved,
                repair_iterations: run.repair_iterations as usize,
            },
        );
        artifacts.review_surface = Some(surface.clone());

        let capsule = EvidenceCapsuleV3::build(
            &run,
            &contribution,
            &permit,
            &report,
            &spec,
            &graph,
            artifacts.challenge.as_ref(),
            artifacts.reproduction.clone(),
            Some(&surface),
        );
        artifacts.capsule = Some(capsule.clone());
        self.save_artifacts(run_id, &artifacts)?;
        Ok(capsule)
    }

    /// Human review of the exact candidate fingerprint.
    pub async fn review(&self, run_id: &str) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if run.state != RunState::ReadyForReview {
            return Err(ContribError::Config(format!(
                "run {} is not awaiting review (state {})",
                run_id,
                run.state.as_str()
            )));
        }
        let mut artifacts = self.artifacts(run_id)?;
        let capsule = artifacts.capsule.clone().ok_or_else(|| {
            ContribError::Config("evidence capsule missing — package first".into())
        })?;
        let contribution = artifacts
            .contribution
            .clone()
            .ok_or_else(|| ContribError::Config("contribution missing".into()))?;
        let decision = self
            .env
            .human_review(&contribution, &run.repository, &capsule)
            .await?;

        if decision.decision.is_approved() {
            let approved_fp = decision.approved_fingerprint.ok_or_else(|| {
                ContribError::Config("approval returned no candidate fingerprint".into())
            })?;
            run.review_fingerprint = Some(approved_fp.clone());
            run.review_decided_at = Some(Utc::now());
            self.memory.update_run(&run)?;
            if let Some(capsule) = artifacts.capsule.as_mut() {
                capsule.bind_review(&approved_fp);
            }
            self.save_artifacts(run_id, &artifacts)?;
            run = self
                .memory
                .transition_run(
                    run_id,
                    RunState::Approved,
                    "review",
                    "human approved candidate",
                )?
                .0;
            Ok(run)
        } else {
            let detail = if decision.decision.is_rejected() {
                "human rejected the candidate"
            } else {
                "human skipped the candidate"
            };
            self.terminal(run_id, RunState::Cancelled, "review", detail)
        }
    }

    /// Submit the draft PR — only with the explicit `--submit` capability.
    pub async fn submit(&self, run_id: &str) -> Result<ContributionRun> {
        let run = self.load(run_id)?;
        self.enforce_live(&run)?;
        if !self.config.submit_capable {
            return Err(ContribError::Config(
                "submission requires the --submit capability grant".into(),
            ));
        }
        if run.state != RunState::Approved {
            return Err(ContribError::Config(format!(
                "run {} is not approved (state {})",
                run_id,
                run.state.as_str()
            )));
        }
        let artifacts = self.artifacts(run_id)?;
        let capsule = artifacts
            .capsule
            .clone()
            .ok_or_else(|| ContribError::Config("evidence capsule missing".into()))?;
        let contribution = artifacts
            .contribution
            .clone()
            .ok_or_else(|| ContribError::Config("contribution missing".into()))?;
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing".into()))?;
        let repository = self.env.fetch_repository(&run.repository).await?;

        let result = self
            .env
            .submit(&contribution, &repository, &capsule, &run, &permit)
            .await?;

        // Seal the audit record inside the run binding.
        let record = AdmissionAuditRecord::from_attempt_with_run(
            &repository,
            &contribution,
            AdmissionAuditStage::HumanReview,
            AdmissionAuditDecision::Approved,
            format!(
                "run {} submitted as draft PR #{}",
                run.run_id, result.pr_number
            ),
            Some(&permit),
            None,
            capsule.checks.clone(),
            Utc::now(),
            Some(&run.run_id),
        );
        if let Err(error) = self.memory.record_admission_audit(record) {
            // The PR already exists; surface the audit failure loudly rather
            // than pretending the submission never happened.
            return Err(ContribError::Config(format!(
                "draft PR #{} created but audit append failed: {error}",
                result.pr_number
            )));
        }

        let mut run = run;
        run.draft_pr_number = Some(result.pr_number);
        run.draft_pr_url = Some(result.pr_url);
        self.memory.update_run(&run)?;
        run = self
            .memory
            .transition_run(run_id, RunState::Submitted, "submit", "draft PR created")?
            .0;
        Ok(run)
    }

    /// Drive a run from its current state to a terminal/resting state.
    /// Stops at `ready_for_review` when human review is interactive — the
    /// caller invokes `review` and `submit` explicitly.
    pub async fn drive(&self, run_id: &str) -> Result<ContributionRun> {
        self.drive_inner(run_id, true).await
    }

    /// Drive through evidence packaging only — stops before human review.
    /// Used by `--dry-run` so no interactive prompt and no write occurs.
    pub async fn drive_to_evidence(&self, run_id: &str) -> Result<ContributionRun> {
        self.drive_inner(run_id, false).await
    }

    async fn drive_inner(&self, run_id: &str, include_review: bool) -> Result<ContributionRun> {
        let mut run = self.load(run_id)?;
        if run.state == RunState::Discovered {
            run = self.authorize(run_id).await?;
        }
        if run.state == RunState::Authorized {
            self.prepare(run_id).await?;
            run = self.load(run_id)?;
        }
        if run.state == RunState::Prepared {
            self.understand(run_id).await?;
            run = self.reproduce_and_plan(run_id).await?;
        }
        if run.state == RunState::Planned {
            run = self.solve(run_id).await?;
        }
        if run.state == RunState::Executing {
            self.validate(run_id).await?;
            run = self.load(run_id)?;
        }
        // Validating → challenge, with the bounded repair loop:
        // Repairing → re-validate → re-challenge until ready, blocked, or
        // the repair budget is exhausted.
        loop {
            match run.state {
                RunState::Validating => {
                    run = self.challenge_and_maybe_repair(run_id).await?;
                }
                RunState::Repairing => {
                    self.validate(run_id).await?;
                    run = self.load(run_id)?;
                }
                _ => break,
            }
        }
        if run.state == RunState::ReadyForReview {
            self.package_evidence(run_id).await?;
            run = self.load(run_id)?;
        }
        if include_review {
            if run.state == RunState::ReadyForReview {
                run = self.review(run_id).await?;
            }
            if run.state == RunState::Approved && self.config.submit_capable {
                run = self.submit(run_id).await?;
            }
        }
        Ok(run)
    }
}

/// Maintainer label consent for an issue view — deterministic policy mirror
/// of `RepositoryConsent::from_issue_with_labels`.
fn label_consent(issue: &RunIssueView) -> Option<RepositoryConsent> {
    if !issue.state.eq_ignore_ascii_case("open") {
        return None;
    }
    let label = issue.labels.iter().find(|label| {
        MAINTAINER_APPROVAL_LABELS
            .iter()
            .any(|allowed| label.eq_ignore_ascii_case(allowed))
    })?;
    Some(RepositoryConsent::from_label(issue.number, label))
}

/// Repo-relative file paths present in the workspace, for solver context.
fn workspace_files(workspace: &RunWorkspace) -> Vec<String> {
    let mut paths = Vec::new();
    collect_paths(workspace.root(), workspace.root(), &mut paths, 0);
    paths
}

fn collect_paths(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 6 || out.len() > 500 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_paths(root, &path, out, depth + 1);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Compact diff description for the challenger — paths and sizes only,
/// never full contents.
async fn diff_summary(workspace: &RunWorkspace) -> Result<String> {
    let changed = workspace.changed_files().await?;
    let mut lines = Vec::new();
    for file in &changed {
        lines.push(format!(
            "{} (+{}/-{}{})",
            file.path,
            file.lines_added,
            file.lines_deleted,
            if file.is_binary { ", binary" } else { "" }
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Severity;
    use crate::orchestrator::memory::Memory;
    use crate::orchestrator::review_gate::{ReviewAction, ReviewDecision};
    use chrono::Duration;
    use std::sync::Mutex as StdMutex;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repository() -> Repository {
        Repository {
            owner: "octo".into(),
            name: "repo".into(),
            full_name: "octo/repo".into(),
            description: None,
            language: Some("Rust".into()),
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
        }
    }

    const MANIFEST: &str = "schema_version: 2\nenabled: true\nallowed_paths:\n  - src/**\nmax_files: 5\nmax_changed_lines: 200\nrequired_checks:\n  - cargo test\n";

    /// Deterministic fake environment: manifest consent, trivial snapshot,
    /// scripted solver/challenger, auto-approving human.
    struct FakeEnv {
        approve: bool,
        submit_called: StdMutex<bool>,
        challenge_findings: Vec<ChallengeFinding>,
        manifest: Option<&'static str>,
        reproduce_argv: Vec<Vec<String>>,
    }

    impl FakeEnv {
        fn new() -> Self {
            Self {
                approve: true,
                submit_called: StdMutex::new(false),
                challenge_findings: vec![],
                manifest: Some(MANIFEST),
                reproduce_argv: vec![],
            }
        }
    }

    #[async_trait]
    impl RunEnvironment for FakeEnv {
        async fn fetch_consent_manifest(&self, _repo: &str, path: &str) -> Result<Option<String>> {
            Ok(self
                .manifest
                .filter(|_| path == CONSENT_PATHS[0])
                .map(str::to_string))
        }

        async fn fetch_issue(&self, _repo: &str, issue: i64) -> Result<RunIssueView> {
            Ok(RunIssueView {
                number: issue,
                title: "fix the thing".into(),
                body: "the thing is broken".into(),
                state: "open".into(),
                labels: vec![],
                maintainer_comments: vec![],
            })
        }

        async fn fetch_repository(&self, _repo: &str) -> Result<Repository> {
            Ok(repository())
        }

        async fn attest_base_sha(&self, _repo: &str, _issue: Option<i64>) -> Result<String> {
            Ok(SHA.to_string())
        }

        async fn fetch_base_snapshot(
            &self,
            _repo: &str,
            _base_sha: &str,
        ) -> Result<Vec<(String, Vec<u8>)>> {
            Ok(vec![("src/lib.rs".into(), b"pub fn a() {}\n".to_vec())])
        }

        async fn task_inputs(&self, _repo: &str, _issue: Option<i64>) -> Result<OwnedTaskInputs> {
            Ok(OwnedTaskInputs {
                issue_title: "fix the thing".into(),
                issue_body: "the thing is broken".into(),
                maintainer_comments: vec![],
                policy_excerpt: "allowed: src/".into(),
            })
        }

        async fn reproduction_commands(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
        ) -> Result<Vec<Vec<String>>> {
            Ok(self.reproduce_argv.clone())
        }

        async fn solve(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            _paths: &[String],
        ) -> Result<SolverCandidate> {
            Ok(SolverCandidate {
                title: "fix: the thing".into(),
                description: "fixes it".into(),
                commit_message: "fix: the thing".into(),
                contribution_type: ContributionType::CodeQuality,
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
                changes: vec![FileChange {
                    path: "src/lib.rs".into(),
                    original_content: Some("pub fn a() {}\n".into()),
                    new_content: "pub fn a() {}\npub fn b() {}\n".into(),
                    is_new_file: false,
                    is_deleted: false,
                }],
                tests_added: vec![],
            })
        }

        async fn repair(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            _findings: &[ChallengeFinding],
            _paths: &[String],
        ) -> Result<Vec<FileChange>> {
            Ok(vec![FileChange {
                path: "src/lib.rs".into(),
                original_content: None,
                new_content: "pub fn a() {}\npub fn b() {}\npub fn c() {}\n".into(),
                is_new_file: false,
                is_deleted: false,
            }])
        }

        async fn challenge(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            _diff: &str,
        ) -> Result<Vec<ChallengeFinding>> {
            Ok(self.challenge_findings.clone())
        }

        async fn human_review(
            &self,
            _contribution: &Contribution,
            _repo_name: &str,
            evidence: &EvidenceCapsuleV3,
        ) -> Result<RunReviewDecision> {
            Ok(RunReviewDecision {
                decision: ReviewDecision::new(if self.approve {
                    ReviewAction::Approve
                } else {
                    ReviewAction::Reject
                }),
                approved_fingerprint: self
                    .approve
                    .then(|| evidence.contribution_fingerprint.clone()),
            })
        }

        async fn submit(
            &self,
            _contribution: &Contribution,
            repo: &Repository,
            _evidence: &EvidenceCapsuleV3,
            _run: &ContributionRun,
            _permit: &ContributionPermit,
        ) -> Result<PrResult> {
            *self.submit_called.lock().unwrap() = true;
            Ok(PrResult {
                repo: repo.clone(),
                contribution: _contribution.clone(),
                pr_number: 42,
                pr_url: "https://github.com/octo/repo/pull/42".into(),
                status: crate::core::models::PrStatus::Open,
                created_at: Utc::now(),
                branch_name: "fix/test".into(),
                fork_full_name: "me/repo".into(),
            })
        }

        fn solver_model_label(&self) -> Option<String> {
            Some("solver-test".into())
        }
        fn challenger_model_label(&self) -> Option<String> {
            Some("challenger-test".into())
        }
    }

    fn setup() -> (tempfile::TempDir, Memory) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open_in_memory().unwrap();
        (dir, memory)
    }

    fn new_run(memory: &Memory) -> ContributionRun {
        let run = ContributionRun::new("octo/repo", Some(7), SHA, Utc::now() + Duration::hours(1));
        memory.insert_run(&run).unwrap();
        run
    }

    fn executor<'a>(
        memory: &'a Memory,
        env: &'a dyn RunEnvironment,
        root: &Path,
        submit: bool,
    ) -> RunExecutor<'a> {
        RunExecutor::new(
            memory,
            env,
            RunExecConfig {
                runs_root: root.to_path_buf(),
                submit_capable: submit,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn authorize_without_consent_needs_authorization() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            manifest: None,
            ..FakeEnv::new()
        };
        // Manifest None but issue has no label either.
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.authorize(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::NeedsAuthorization);
    }

    #[tokio::test]
    async fn label_consent_authorizes_when_manifest_absent() {
        let (dir, memory) = setup();
        let mut env = FakeEnv {
            manifest: None,
            ..FakeEnv::new()
        };
        // Issue carries a maintainer approval label.
        let run = new_run(&memory);
        // Patch fetch_issue to return a labeled issue — use a wrapper env.
        struct LabeledEnv(FakeEnv);
        #[async_trait]
        impl RunEnvironment for LabeledEnv {
            async fn fetch_consent_manifest(&self, r: &str, p: &str) -> Result<Option<String>> {
                self.0.fetch_consent_manifest(r, p).await
            }
            async fn fetch_issue(&self, r: &str, i: i64) -> Result<RunIssueView> {
                let mut v = self.0.fetch_issue(r, i).await?;
                v.labels = vec!["contribai-approved".into()];
                Ok(v)
            }
            async fn fetch_repository(&self, r: &str) -> Result<Repository> {
                self.0.fetch_repository(r).await
            }
            async fn attest_base_sha(&self, r: &str, i: Option<i64>) -> Result<String> {
                self.0.attest_base_sha(r, i).await
            }
            async fn fetch_base_snapshot(
                &self,
                r: &str,
                s: &str,
            ) -> Result<Vec<(String, Vec<u8>)>> {
                self.0.fetch_base_snapshot(r, s).await
            }
            async fn task_inputs(&self, r: &str, i: Option<i64>) -> Result<OwnedTaskInputs> {
                self.0.task_inputs(r, i).await
            }
            async fn solve(&self, r: &str, s: &TaskSpec, p: &[String]) -> Result<SolverCandidate> {
                self.0.solve(r, s, p).await
            }
            async fn repair(
                &self,
                r: &str,
                s: &TaskSpec,
                f: &[ChallengeFinding],
                p: &[String],
            ) -> Result<Vec<FileChange>> {
                self.0.repair(r, s, f, p).await
            }
            async fn challenge(
                &self,
                r: &str,
                s: &TaskSpec,
                d: &str,
            ) -> Result<Vec<ChallengeFinding>> {
                self.0.challenge(r, s, d).await
            }
            async fn human_review(
                &self,
                c: &Contribution,
                n: &str,
                e: &EvidenceCapsuleV3,
            ) -> Result<RunReviewDecision> {
                self.0.human_review(c, n, e).await
            }
            async fn submit(
                &self,
                c: &Contribution,
                r: &Repository,
                e: &EvidenceCapsuleV3,
                run: &ContributionRun,
                p: &ContributionPermit,
            ) -> Result<PrResult> {
                self.0.submit(c, r, e, run, p).await
            }
        }
        let labeled = LabeledEnv(env);
        let exec = executor(&memory, &labeled, dir.path(), false);
        let run = exec.authorize(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Authorized);
        assert!(run.permit_id.is_some());
        env = labeled.0;
        let _ = env;
    }

    #[tokio::test]
    async fn full_drive_reaches_ready_for_review_without_submit() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        // Auto-approved human → Approved; submit_capable=false → stops.
        assert_eq!(run.state, RunState::Approved);
        let artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        assert!(artifacts.capsule.is_some());
        assert_eq!(
            artifacts.capsule.as_ref().unwrap().review_fingerprint,
            run.review_fingerprint
        );
    }

    #[tokio::test]
    async fn submit_requires_capability() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Approved);
        let err = exec.submit(&run.run_id).await.unwrap_err();
        assert!(err.to_string().contains("--submit"));
        assert!(!*env.submit_called.lock().unwrap());
    }

    #[tokio::test]
    async fn full_drive_submits_with_capability() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), true);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Submitted);
        assert_eq!(run.draft_pr_number, Some(42));
        assert!(*env.submit_called.lock().unwrap());
        // Audit chain recorded a run-bound schema-2 record.
        let audits = memory
            .get_admission_audits(Some("octo/repo"), None, 10)
            .unwrap();
        assert!(audits
            .iter()
            .any(|a| a.run_id.as_deref() == Some(run.run_id.as_str())));
    }

    #[tokio::test]
    async fn challenger_concerns_trigger_bounded_repair() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            challenge_findings: vec![ChallengeFinding {
                severity: crate::core::challenge::ChallengeSeverity::High,
                category: crate::core::challenge::ChallengeCategory::MissingEdgeCase,
                summary: "edge case".into(),
                file_path: None,
                resolved: false,
            }],
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        // Repair ran once, then challenger still reports the same concern
        // (fake returns identical findings) → budget exhausts → blocked.
        assert_eq!(run.state, RunState::Blocked);
        assert!(run.repair_iterations > 0);
    }

    #[tokio::test]
    async fn rejected_review_cancels_run() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            approve: false,
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Cancelled);
    }

    #[tokio::test]
    async fn attested_sha_mismatch_blocks() {
        let (dir, memory) = setup();
        struct BadSha(FakeEnv);
        #[async_trait]
        impl RunEnvironment for BadSha {
            async fn fetch_consent_manifest(&self, r: &str, p: &str) -> Result<Option<String>> {
                self.0.fetch_consent_manifest(r, p).await
            }
            async fn fetch_issue(&self, r: &str, i: i64) -> Result<RunIssueView> {
                self.0.fetch_issue(r, i).await
            }
            async fn fetch_repository(&self, r: &str) -> Result<Repository> {
                self.0.fetch_repository(r).await
            }
            async fn attest_base_sha(&self, _r: &str, _i: Option<i64>) -> Result<String> {
                Ok("f".repeat(40))
            }
            async fn fetch_base_snapshot(
                &self,
                r: &str,
                s: &str,
            ) -> Result<Vec<(String, Vec<u8>)>> {
                self.0.fetch_base_snapshot(r, s).await
            }
            async fn task_inputs(&self, r: &str, i: Option<i64>) -> Result<OwnedTaskInputs> {
                self.0.task_inputs(r, i).await
            }
            async fn solve(&self, r: &str, s: &TaskSpec, p: &[String]) -> Result<SolverCandidate> {
                self.0.solve(r, s, p).await
            }
            async fn repair(
                &self,
                r: &str,
                s: &TaskSpec,
                f: &[ChallengeFinding],
                p: &[String],
            ) -> Result<Vec<FileChange>> {
                self.0.repair(r, s, f, p).await
            }
            async fn challenge(
                &self,
                r: &str,
                s: &TaskSpec,
                d: &str,
            ) -> Result<Vec<ChallengeFinding>> {
                self.0.challenge(r, s, d).await
            }
            async fn human_review(
                &self,
                c: &Contribution,
                n: &str,
                e: &EvidenceCapsuleV3,
            ) -> Result<RunReviewDecision> {
                self.0.human_review(c, n, e).await
            }
            async fn submit(
                &self,
                c: &Contribution,
                r: &Repository,
                e: &EvidenceCapsuleV3,
                run: &ContributionRun,
                p: &ContributionPermit,
            ) -> Result<PrResult> {
                self.0.submit(c, r, e, run, p).await
            }
        }
        let env = BadSha(FakeEnv::new());
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.authorize(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }
}
