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

use std::collections::BTreeMap;

use crate::core::admission::{
    evaluate_run_requirements, is_full_commit_sha, repository_path_error, AdmissionAuditDecision,
    AdmissionAuditRecord, AdmissionAuditStage, AdmissionController, ContributionPermit,
    RepositoryConsent, CONSENT_PATHS, MAINTAINER_APPROVAL_LABELS,
};
use crate::core::challenge::{ChallengeFinding, ChallengeReport};
use crate::core::error::{ContribError, Result};
use crate::core::evidence_v3::{EvidenceCapsuleV3, ReproductionEvidence};
use crate::core::materialization::{
    materialization_gaps, BaseSnapshot, MaterializationReport, MaterializedWorkspace,
};
use crate::core::models::{
    Contribution, ContributionType, FileChange, Finding, PrResult, Repository,
};
use crate::core::review_surface::{ChangedFile, ReviewContext, ReviewSurface};
use crate::core::run::{ContributionRun, RunState};
use crate::core::task_spec::{TaskInputs, TaskSpec};
use crate::core::unified_diff::{self, DiffBundle};
use crate::core::validation_graph::{
    CheckMechanism, CheckResult, GraphVerdict, ValidationCheck, ValidationGraph,
};
use crate::exec::adapters;
use crate::exec::runner::BoundedRunner;
use crate::exec::workspace::{RunWorkspace, WorkspaceView};
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

/// Everything the challenger needs to attack the actual candidate —
/// unified hunks, deterministic validation evidence, reproduction
/// results, and bounded read access for surrounding context.
pub struct ChallengeInput<'a> {
    /// Unified hunks of the exact candidate under review.
    pub diff: &'a DiffBundle,
    /// The validation graph just produced for this candidate.
    pub validation: &'a ValidationGraph,
    /// Reproduction evidence (base leg plus candidate leg when run).
    pub reproduction: Option<&'a ReproductionEvidence>,
    /// Bounded read access to the materialized workspace.
    pub workspace: WorkspaceView<'a>,
}

/// Everything the repair loop needs to fix the actual candidate.
pub struct RepairRequest<'a> {
    /// Unified hunks of the current (failing) candidate.
    pub diff: &'a DiffBundle,
    /// Challenger findings from the last report (all, resolved included —
    /// the env decides which to act on).
    pub findings: &'a [ChallengeFinding],
    /// Failed required validation checks with bounded output excerpts.
    pub failing_checks: Vec<&'a ValidationCheck>,
    /// Bounded read access to the materialized workspace.
    pub workspace: WorkspaceView<'a>,
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

    /// Repository file snapshot at `base_sha` — the materialization
    /// fallback and context source. Every file not returned must be
    /// accounted for in [`BaseSnapshot::skipped`]; `truncated` marks a
    /// listing that provably did not cover the tree.
    async fn fetch_base_snapshot(&self, repo: &str, base_sha: &str) -> Result<BaseSnapshot>;

    /// Materialize the run workspace at the attested base SHA.
    ///
    /// The default writes the API snapshot — which may be incomplete.
    /// Environments with git transport should override with a clone or
    /// worktree: a truncated snapshot is not a valid test workspace. The
    /// executor judges completeness itself via [`materialization_gaps`]
    /// and blocks on missing in-scope files, manifests, or tests — the
    /// environment cannot self-certify.
    async fn materialize_workspace(
        &self,
        repo: &str,
        base_sha: &str,
        runs_root: &Path,
        run_id: &str,
    ) -> Result<MaterializedWorkspace> {
        let snapshot = self.fetch_base_snapshot(repo, base_sha).await?;
        let workspace =
            RunWorkspace::materialize(runs_root, run_id, base_sha, &snapshot.files).await?;
        Ok(MaterializedWorkspace {
            workspace,
            report: MaterializationReport {
                strategy: "snapshot".into(),
                materialized_files: snapshot.files.len(),
                skipped: snapshot.skipped,
                truncated: snapshot.truncated,
                gaps: Vec::new(),
            },
        })
    }

    /// Maintainer-authored task inputs for the TaskSpec.
    async fn task_inputs(&self, repo: &str, issue: Option<i64>) -> Result<OwnedTaskInputs>;

    /// Reproduction commands to run at base revision — and again on the
    /// candidate. The workspace view exposes the real materialized tree so
    /// environments derive commands from what is actually present. Empty =
    /// no reproduction surface (docs-only, environment cannot execute).
    async fn reproduction_commands(
        &self,
        _repo: &str,
        _spec: &TaskSpec,
        _workspace: &WorkspaceView<'_>,
    ) -> Result<Vec<Vec<String>>> {
        Ok(Vec::new())
    }

    /// Solver model produces the candidate. `workspace` is the bounded
    /// read interface into the materialized tree — solvers get real file
    /// contents, not just paths.
    async fn solve(
        &self,
        repo: &str,
        spec: &TaskSpec,
        workspace: &WorkspaceView<'_>,
    ) -> Result<SolverCandidate>;

    /// Bounded repair: produce revised changes for unresolved findings.
    /// The request carries the real current diff, the failing checks with
    /// output excerpts, and read access to the workspace.
    async fn repair(
        &self,
        repo: &str,
        spec: &TaskSpec,
        request: &RepairRequest<'_>,
    ) -> Result<Vec<FileChange>>;

    /// Independent challenger produces adversarial findings for the
    /// candidate's *actual unified diff*, with validation evidence and
    /// workspace context. The executor seals the report — the model cannot
    /// mark its own output complete.
    async fn challenge(
        &self,
        repo: &str,
        spec: &TaskSpec,
        input: &ChallengeInput<'_>,
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
    /// How the workspace was materialized and how complete it is.
    #[serde(default)]
    pub materialization: Option<MaterializationReport>,
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

        let mut artifacts = self.artifacts(run_id)?;
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing — run authorize first".into()))?;

        let mut materialized = self
            .env
            .materialize_workspace(
                &run.repository,
                &run.base_sha,
                &self.config.runs_root,
                run_id,
            )
            .await?;
        materialized.report.gaps = materialization_gaps(&materialized.report, &permit);
        artifacts.materialization = Some(materialized.report.clone());
        self.save_artifacts(run_id, &artifacts)?;

        if !materialized.report.is_complete() {
            let detail = format!(
                "workspace is not a complete checkout of the attested base: {}",
                materialized.report.gaps.join("; ")
            );
            self.terminal(run_id, RunState::Blocked, "prepare", &detail)?;
            return Err(ContribError::Config(detail));
        }

        self.memory.transition_run(
            run_id,
            RunState::Prepared,
            "prepare",
            &format!(
                "workspace materialized ({} files via {})",
                materialized.report.materialized_files, materialized.report.strategy
            ),
        )?;
        Ok(materialized.workspace)
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

        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        let commands = self
            .env
            .reproduction_commands(&run.repository, &spec, &workspace.view())
            .await?;

        if commands.is_empty() {
            artifacts.reproduction = Some(ReproductionEvidence {
                attempted: false,
                reproduced: false,
                mechanism: "no reproduction surface".into(),
                output_digest: None,
                commands: Vec::new(),
                candidate: None,
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
            let runner = self.runner_for(&workspace, &run, &permit);

            let mut reproduced = false;
            let mut last_digest = None;
            let mut mechanism = String::new();
            for argv in &commands {
                mechanism = format!("command: {}", argv.join(" "));
                match runner.run_indirect(argv, argv).await {
                    Ok(outcome) => {
                        last_digest = Some(outcome.output_digest.clone());
                        // Convention: a reproduction command "reproduces"
                        // the bug when it exits non-zero at base (the
                        // failing behavior is demonstrably present).
                        if outcome.gate == "ran" && !outcome.timed_out && !outcome.passed() {
                            reproduced = true;
                        }
                    }
                    Err(error) => {
                        // A command that cannot even spawn is recorded,
                        // not propagated — the run reports honestly that
                        // reproduction was attempted but did not run.
                        mechanism = format!("command failed to spawn: {}", argv.join(" "));
                        last_digest = None;
                        tracing::warn!(
                            run_id,
                            error = %error,
                            "reproduction command could not start"
                        );
                    }
                }
            }
            artifacts.reproduction = Some(ReproductionEvidence {
                attempted: true,
                reproduced,
                mechanism,
                output_digest: last_digest,
                commands: commands.clone(),
                candidate: None,
            });
            run.reproduction = Some(if reproduced {
                "reproduced".into()
            } else {
                "not_reproduced".into()
            });
            // Reproduction commands legitimately write build/test
            // artifacts (caches, bytecode) into the workspace — fold them
            // into the expected state so the next stage does not flag the
            // command's own output as a foreign edit.
            let mut workspace = workspace;
            workspace.checkpoint().await?;
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
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing — run authorize first".into()))?;

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

        let mut candidate = match self
            .env
            .solve(&run.repository, &spec, &workspace.view())
            .await
        {
            Ok(candidate) => candidate,
            Err(error) => {
                // Solver transport/parse failure: record and fail closed
                // rather than leave the run parked in `executing`.
                let detail = format!("solver failed to produce a candidate: {error}");
                self.terminal(run_id, RunState::Failed, "solve", &detail)?;
                return Err(ContribError::Config(detail));
            }
        };

        // An empty candidate is not a contribution — fail closed rather
        // than packaging evidence for a change that does not exist.
        if candidate.changes.is_empty() && candidate.tests_added.is_empty() {
            return self.terminal(
                run_id,
                RunState::Blocked,
                "solve",
                "solver produced no file changes",
            );
        }

        // Verify and apply the candidate inside the workspace. Scope is
        // evaluated BEFORE any write; preimages bind to what is actually
        // on disk: a solver whose claimed base content does not match is
        // working from hallucinated context and is rejected.
        let prior_preimages = BTreeMap::new();
        if let Err(reason) = bind_preimages(
            &workspace,
            &mut candidate.changes,
            &mut candidate.tests_added,
            &prior_preimages,
        ) {
            return self.terminal(run_id, RunState::Blocked, "solve", &reason);
        }
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
        let repository = self.env.fetch_repository(&run.repository).await?;
        let report = AdmissionController::evaluate(&repository, &contribution, &permit, Utc::now());
        if !report.allowed {
            let reason = report
                .violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return self.terminal(
                run_id,
                RunState::Blocked,
                "solve",
                &format!("candidate violates permit: {reason}"),
            );
        }
        for change in contribution
            .changes
            .iter()
            .chain(contribution.tests_added.iter())
        {
            workspace.write_file(&change.path, change.new_content.as_bytes())?;
        }
        let mut workspace = workspace;
        workspace.checkpoint().await?;

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

        let mut workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
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
                // adapter's optional flag. Permit names are canonical
                // (`cargo_test`); adapter names are display form
                // (`cargo test`) — compare in canonical form.
                let required = check.required
                    || permit.required_checks.iter().any(|name| {
                        *name == crate::core::validation_graph::canonical_check_name(&check.name)
                    });
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

        // Candidate reproduction leg: when the bug was reproduced at base,
        // the same commands must now pass. A patch that does not resolve
        // the reproduced behavior fails closed — required when the base
        // leg demonstrably reproduced the bug.
        if let Some(repro) = artifacts.reproduction.clone() {
            if repro.attempted && !repro.commands.is_empty() {
                let mut resolved = true;
                let mut mechanism = String::new();
                let mut last_digest = None;
                let mut last_excerpt = String::new();
                let mut spawn_error = false;
                for argv in &repro.commands {
                    mechanism = format!("command: {}", argv.join(" "));
                    match runner.run_indirect(argv, argv).await {
                        Ok(outcome) => {
                            last_digest = Some(outcome.output_digest.clone());
                            last_excerpt =
                                format!("{}{}", outcome.stdout_excerpt, outcome.stderr_excerpt);
                            if outcome.gate != "ran" || outcome.timed_out || !outcome.passed() {
                                resolved = false;
                            }
                        }
                        Err(_) => {
                            resolved = false;
                            spawn_error = true;
                        }
                    }
                }
                let first_argv = repro.commands.first().cloned().unwrap_or_default();
                artifacts.reproduction = Some(ReproductionEvidence {
                    candidate: Some(crate::core::evidence_v3::CandidateReproduction {
                        resolved,
                        output_digest: last_digest,
                        mechanism: mechanism.clone(),
                    }),
                    ..repro
                });
                let mut node = ValidationCheck::new(
                    "reproduction",
                    "runtime",
                    CheckMechanism::Command { argv: first_argv },
                    // Required only when the base leg actually reproduced
                    // the bug — a never-reproduced issue cannot certify a
                    // fix.
                    repro.reproduced,
                );
                node.finish(
                    if resolved {
                        CheckResult::Pass
                    } else {
                        CheckResult::Fail
                    },
                    if resolved {
                        format!("{mechanism}: reproduced behavior resolved on candidate")
                    } else if spawn_error {
                        format!("{mechanism}: command failed to spawn on candidate")
                    } else {
                        format!("{mechanism}: reproduced behavior still present")
                    },
                    Some(last_excerpt),
                    None,
                );
                graph.push(node);
            }
        }

        // Validation commands legitimately write build/test artifacts into
        // the workspace (target/, __pycache__, .pytest_cache). Refresh the
        // checkpoint so the *next* stage's foreign-edit check compares
        // against post-validation state rather than flagging the checks'
        // own output.
        workspace.checkpoint().await?;

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

        let candidate_fp = run.candidate_fingerprint.clone().unwrap_or_default();
        let challenger_label = self
            .env
            .challenger_model_label()
            .unwrap_or_else(|| "unknown".into());

        // The challenger reviews the actual unified diff of the exact
        // candidate — bound preimages vs current workspace content — plus
        // validation and reproduction evidence, with bounded workspace
        // access for surrounding context.
        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        if workspace.has_unexpected_changes().await? {
            return self.terminal(
                run_id,
                RunState::Failed,
                "challenge",
                "workspace has unexpected changes before challenge",
            );
        }
        let contribution = artifacts
            .contribution
            .clone()
            .ok_or_else(|| ContribError::Config("contribution missing".into()))?;
        let diff = candidate_diff(&workspace, &contribution);
        let input = ChallengeInput {
            diff: &diff,
            validation: &graph,
            reproduction: artifacts.reproduction.as_ref(),
            workspace: workspace.view(),
        };
        let findings = match self.env.challenge(&run.repository, &spec, &input).await {
            Ok(findings) => findings,
            Err(error) => {
                // A challenger that cannot run is not a clean review:
                // record the not-run report and fail closed.
                artifacts.challenge =
                    Some(ChallengeReport::not_run(&candidate_fp, &challenger_label));
                self.save_artifacts(run_id, &artifacts)?;
                let detail = format!("challenger could not produce findings: {error}");
                return self.terminal(run_id, RunState::Blocked, "challenge", &detail);
            }
        };
        let report = ChallengeReport::completed(findings, &candidate_fp, &challenger_label);
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
        let permit = artifacts
            .permit
            .clone()
            .ok_or_else(|| ContribError::Config("permit missing".into()))?;
        let findings = artifacts
            .challenge
            .as_ref()
            .map(|c| c.findings.clone())
            .unwrap_or_default();
        let failing_checks: Vec<ValidationCheck> = artifacts
            .validation
            .as_ref()
            .map(|graph| {
                graph
                    .checks
                    .iter()
                    .filter(|check| {
                        check.required
                            && matches!(check.result, CheckResult::Fail | CheckResult::Skipped)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;

        // The repair sees the actual current diff, the unresolved
        // findings, and the real failing checks — not a path list.
        let prior = artifacts
            .contribution
            .clone()
            .ok_or_else(|| ContribError::Config("contribution missing".into()))?;
        let diff = candidate_diff(&workspace, &prior);
        let failing_refs: Vec<&ValidationCheck> = failing_checks.iter().collect();
        let request = RepairRequest {
            diff: &diff,
            findings: &findings,
            failing_checks: failing_refs,
            workspace: workspace.view(),
        };
        let mut revised = match self.env.repair(&run.repository, &spec, &request).await {
            Ok(revised) => revised,
            Err(error) => {
                let detail = format!("repair failed to produce changes: {error}");
                self.terminal(run_id, RunState::Failed, "repair", &detail)?;
                return Err(ContribError::Config(detail));
            }
        };

        if revised.is_empty() {
            return self.terminal(
                run_id,
                RunState::Blocked,
                "repair",
                "repair produced no file changes",
            );
        }

        // Bind preimages: for files already touched, the base preimage is
        // the recorded one from the prior change set — the workspace now
        // holds the candidate, not the base.
        let prior_preimages: BTreeMap<String, Option<String>> = prior
            .changes
            .iter()
            .chain(prior.tests_added.iter())
            .map(|change| (change.path.clone(), change.original_content.clone()))
            .collect();
        let mut revised_tests = Vec::new();
        if let Err(reason) = bind_preimages(
            &workspace,
            &mut revised,
            &mut revised_tests,
            &prior_preimages,
        ) {
            return self.terminal(run_id, RunState::Blocked, "repair", &reason);
        }

        // The repaired candidate replaces the staged contribution — the
        // prior human review (if any) is invalidated by the new fingerprint.
        if let Some(contribution) = artifacts.contribution.as_mut() {
            contribution.changes = revised;
            contribution.generated_at = Utc::now();
        }
        let repository = self.env.fetch_repository(&run.repository).await?;
        if let Some(contribution) = artifacts.contribution.as_ref() {
            let report =
                AdmissionController::evaluate(&repository, contribution, &permit, Utc::now());
            if !report.allowed {
                let reason = report
                    .violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                return self.terminal(
                    run_id,
                    RunState::Blocked,
                    "repair",
                    &format!("repair violates permit: {reason}"),
                );
            }
            for change in &contribution.changes {
                workspace.write_file(&change.path, change.new_content.as_bytes())?;
            }
        }
        let mut workspace = workspace;
        workspace.checkpoint().await?;

        // Invalidate every artifact derived from the old candidate —
        // validation, challenge, review surface, capsule, and review
        // binding all describe the pre-repair diff.
        artifacts.validation = None;
        artifacts.challenge = None;
        artifacts.review_surface = None;
        artifacts.capsule = None;
        if let Some(repro) = artifacts.reproduction.as_mut() {
            repro.candidate = None;
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

        // Deterministic review-cost surface from the candidate itself:
        // recorded base preimages vs the current workspace contents. This
        // is exact — build artifacts produced by validation commands never
        // leak into the review surface.
        let workspace = RunWorkspace::open_existing(&self.config.runs_root, run_id)?;
        if workspace.has_unexpected_changes().await? {
            return self
                .terminal(
                    run_id,
                    RunState::Failed,
                    "evidence",
                    "workspace has unexpected changes before evidence packaging",
                )
                .map(|_| unreachable!());
        }
        let changed: Vec<ChangedFile> = contribution
            .changes
            .iter()
            .chain(contribution.tests_added.iter())
            .map(|change| {
                let before = change.original_content.clone().unwrap_or_default();
                let after = workspace
                    .read_file(&change.path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|_| change.new_content.clone());
                let (added, deleted) =
                    crate::core::review_surface::line_diff_counts(&before, &after);
                ChangedFile {
                    path: change.path.clone(),
                    lines_added: added,
                    lines_deleted: deleted,
                    is_binary: after.as_bytes().contains(&0),
                }
            })
            .collect();
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

        let mut capsule = EvidenceCapsuleV3::build(
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
        capsule.materialization = artifacts.materialization.clone();
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

/// Unified diff of the exact candidate: recorded base preimages vs the
/// current workspace contents (read back, not the solver's claim).
/// Deterministic and bounded — truncation is marked, never silent.
fn candidate_diff(workspace: &RunWorkspace, contribution: &Contribution) -> DiffBundle {
    let mut triples: Vec<(String, String, String)> = Vec::new();
    for change in contribution
        .changes
        .iter()
        .chain(contribution.tests_added.iter())
    {
        let before = change.original_content.clone().unwrap_or_default();
        // Read back the workspace content — the diff describes what is
        // actually on disk, so foreign edits cannot hide inside a
        // solver-reported postimage.
        let after = workspace
            .read_file(&change.path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|_| change.new_content.clone());
        triples.push((change.path.clone(), before, after));
    }
    unified_diff::render(&triples)
}

/// Verify and bind change preimages to on-disk truth.
///
/// - Rejects deletions and policy-invalid paths outright.
/// - A solver-supplied `original_content` must equal the base bytes —
///   claiming a different base means the model hallucinated its context.
/// - Missing `original_content` is filled from the workspace (or the
///   recorded base preimage for files already changed by a prior
///   candidate, where the workspace holds post-image content).
/// - `is_new_file` is normalized to the bound truth.
fn bind_preimages(
    workspace: &RunWorkspace,
    changes: &mut [FileChange],
    tests_added: &mut [FileChange],
    prior_preimages: &BTreeMap<String, Option<String>>,
) -> std::result::Result<(), String> {
    for change in changes.iter_mut().chain(tests_added.iter_mut()) {
        if change.is_deleted {
            return Err(format!(
                "candidate deletes {} — file deletions are not supported",
                change.path
            ));
        }
        if let Some(reason) = repository_path_error(&change.path) {
            return Err(format!("unsafe path {:?}: {reason}", change.path));
        }
        // Base bytes: prior preimage when this path was already changed,
        // else the current workspace content (the workspace is at base
        // for solve, or base+candidate for repair).
        let current = workspace
            .resolve(&change.path)
            .ok()
            .filter(|p| p.is_file())
            .and_then(|_| workspace.read_file(&change.path).ok());
        let base: Option<Vec<u8>> = match prior_preimages.get(&change.path) {
            Some(preimage) => preimage.as_ref().map(|text| text.as_bytes().to_vec()),
            None => current.clone(),
        };
        if let Some(claimed) = &change.original_content {
            let claimed_bytes = claimed.as_bytes();
            // During repair a model naturally quotes the current file
            // (base + prior candidate). Accept either the recorded base
            // preimage or the on-disk content — anything else means the
            // model is working from stale or fabricated context.
            let matches_base = base.as_deref().map(|b| b == claimed_bytes).unwrap_or(false);
            let matches_current = prior_preimages.contains_key(&change.path)
                && current
                    .as_deref()
                    .map(|c| c == claimed_bytes)
                    .unwrap_or(false);
            if !matches_base && !matches_current {
                return Err(format!(
                    "claimed preimage for {} does not match the workspace — \
                     solver is working from stale or fabricated context",
                    change.path
                ));
            }
        }
        change.is_new_file = base.is_none();
        change.original_content = base.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    }
    Ok(())
}

// ── Strict model-output parsing ─────────────────────────────────────────
//
// Solver, repair, and challenger output is untrusted model text. Parsing
// fails closed: malformed JSON, missing required fields, or unparseable
// items are errors — never "empty output" that downstream stages could
// mistake for a clean result.

/// Extract the single JSON document from model output.
///
/// Accepts a bare JSON value, a fenced ```json block, or prose with one
/// embedded document (first `{`/`[` through the last `}`/`]`). Whatever is
/// extracted must parse completely — a document that is truncated or
/// malformed is an error, not a partial result.
fn extract_json_document(response: &str) -> Result<serde_json::Value> {
    let trimmed = response.trim();
    let mut candidates: Vec<&str> = vec![
        trimmed,
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim(),
    ];
    let start = trimmed.find(['{', '[']).unwrap_or(usize::MAX);
    let end = trimmed.rfind(['}', ']']);
    if let (Some(end), true) = (end, start != usize::MAX) {
        if end > start {
            candidates.push(&trimmed[start..=end]);
        }
    }
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            return Ok(value);
        }
    }
    Err(ContribError::Config(
        "model output is not a parseable JSON document".into(),
    ))
}

/// Parse one `{path, new_content}` change object strictly.
fn parse_file_change(item: &serde_json::Value, context: &str) -> Result<FileChange> {
    let object = item
        .as_object()
        .ok_or_else(|| ContribError::Config(format!("{context}: change item is not an object")))?;
    let path = object
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| {
            ContribError::Config(format!("{context}: change is missing a non-empty `path`"))
        })?;
    let new_content = object
        .get("new_content")
        .and_then(|v| v.as_str())
        .or_else(|| object.get("content").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            ContribError::Config(format!(
                "{context}: change for {path} is missing `new_content`"
            ))
        })?;
    if new_content.is_empty() {
        return Err(ContribError::Config(format!(
            "{context}: change for {path} has empty content — deletions are not supported"
        )));
    }
    Ok(FileChange {
        path: path.to_string(),
        original_content: object
            .get("original_content")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        new_content: new_content.to_string(),
        is_new_file: object
            .get("is_new_file")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_deleted: object
            .get("is_deleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Parse the `changes`/`tests_added` arrays from a solver/repair document.
fn parse_change_arrays(
    data: &serde_json::Value,
    context: &str,
) -> Result<(Vec<FileChange>, Vec<FileChange>)> {
    let object = data
        .as_object()
        .ok_or_else(|| ContribError::Config(format!("{context}: output is not a JSON object")))?;
    let mut changes = Vec::new();
    let mut tests = Vec::new();
    for (key, sink) in [("changes", &mut changes), ("tests_added", &mut tests)] {
        if let Some(value) = object.get(key) {
            let items = value.as_array().ok_or_else(|| {
                ContribError::Config(format!("{context}: `{key}` is not an array"))
            })?;
            for item in items {
                sink.push(parse_file_change(item, context)?);
            }
        }
    }
    Ok((changes, tests))
}

/// Strictly parse solver output into a [`SolverCandidate`].
///
/// Requires a JSON object with a non-empty `changes` array where every
/// entry has a non-empty `path` and `new_content`. Anything else is an
/// error — a run must not proceed on output it cannot fully account for.
pub fn parse_solver_output(response: &str, spec: &TaskSpec) -> Result<SolverCandidate> {
    let data = extract_json_document(response)?;
    let (changes, tests_added) = parse_change_arrays(&data, "solver output")?;
    if changes.is_empty() && tests_added.is_empty() {
        return Err(ContribError::Config(
            "solver output contains no file changes".into(),
        ));
    }
    let title = data["title"].as_str().unwrap_or(&spec.title).to_string();
    let description = data["description"].as_str().unwrap_or("").to_string();
    let commit_message = data["commit_message"]
        .as_str()
        .unwrap_or(&title)
        .to_string();
    let first_path = changes
        .first()
        .or(tests_added.first())
        .map(|c| c.path.clone())
        .unwrap_or_default();
    Ok(SolverCandidate {
        title,
        description,
        commit_message,
        contribution_type: ContributionType::CodeQuality,
        finding: Finding {
            id: String::new(),
            finding_type: ContributionType::CodeQuality,
            severity: crate::core::models::Severity::Medium,
            title: spec.title.clone(),
            description: spec
                .requirements
                .first()
                .map(|r| r.text.clone())
                .unwrap_or_default(),
            file_path: first_path,
            line_start: None,
            line_end: None,
            suggestion: None,
            confidence: 0.8,
            priority_signals: vec![],
        },
        changes,
        tests_added,
    })
}

/// Strictly parse repair output — a JSON object with `changes` and
/// optionally `tests_added`, non-empty in total.
pub fn parse_repair_output(response: &str) -> Result<Vec<FileChange>> {
    let data = extract_json_document(response)?;
    let (changes, tests) = parse_change_arrays(&data, "repair output")?;
    let mut all = changes;
    all.extend(tests);
    if all.is_empty() {
        return Err(ContribError::Config(
            "repair output contains no file changes".into(),
        ));
    }
    Ok(all)
}

/// Strictly parse challenger output into findings.
///
/// Accepts `{"findings": [...]}` or a bare array. Malformed JSON, a
/// non-array `findings`, or an item without a summary string is an
/// error — an unparseable challenger response must never masquerade as
/// "no findings".
pub fn parse_findings_output(response: &str) -> Result<Vec<ChallengeFinding>> {
    let data = extract_json_document(response)?;
    let items: &Vec<serde_json::Value> = match data.get("findings") {
        Some(value) => value.as_array().ok_or_else(|| {
            ContribError::Config("challenger output: `findings` is not an array".into())
        })?,
        None => data.as_array().ok_or_else(|| {
            ContribError::Config(
                "challenger output is neither an object with `findings` nor an array".into(),
            )
        })?,
    };
    let mut findings = Vec::new();
    for item in items.iter().take(50) {
        let summary = item["summary"].as_str().unwrap_or("").trim().to_string();
        if summary.is_empty() {
            return Err(ContribError::Config(
                "challenger finding is missing a non-empty `summary`".into(),
            ));
        }
        findings.push(ChallengeFinding {
            severity: crate::core::challenge::ChallengeSeverity::parse(
                item["severity"].as_str().unwrap_or("info"),
            ),
            category: crate::core::challenge::ChallengeCategory::parse(
                item["category"].as_str().unwrap_or("other"),
            ),
            summary: crate::core::safe_truncate(&summary, 500).to_string(),
            file_path: item["file_path"].as_str().map(str::to_string),
            resolved: false,
        });
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::materialization::{SkipReason, SkippedFile};
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

    /// Manifest consent: write scope `src/**`, modest budgets.
    const MANIFEST: &str = "schema_version: 2\nenabled: true\nallowed_paths:\n  - src/**\nmax_files: 5\nmax_changed_lines: 200\n";
    /// Variant with a required check no adapter covers for a bare snapshot.
    /// `cargo_test` is the canonical check name (`cargo test` canonicalizes
    /// to it at manifest parse).
    const MANIFEST_STRICT: &str = "schema_version: 2\nenabled: true\nallowed_paths:\n  - src/**\nmax_files: 5\nmax_changed_lines: 200\nrequired_checks:\n  - cargo_test\n";
    /// Variant with unrestricted path scope (protected paths still deny).
    const MANIFEST_ALL: &str = "schema_version: 2\nenabled: true\nallowed_paths:\n  - \"**\"\nmax_files: 5\nmax_changed_lines: 500\n";

    fn default_snapshot() -> Vec<(String, Vec<u8>)> {
        vec![("src/lib.rs".into(), b"pub fn a() {}\n".to_vec())]
    }

    /// Deterministic fake environment: manifest consent, scripted
    /// solver/challenger, auto-approving human. The solver/repair/challenge
    /// implementations exercise the real `WorkspaceView` so tests prove
    /// context reaches the model boundary.
    struct FakeEnv {
        approve: bool,
        submit_called: StdMutex<bool>,
        manifest: Option<&'static str>,
        label: Option<&'static str>,
        attested_sha: Option<String>,
        snapshot_files: Vec<(String, Vec<u8>)>,
        skipped: Vec<SkippedFile>,
        truncated: bool,
        reproduce_argv: Vec<Vec<String>>,
        solve_changes: Option<Vec<FileChange>>,
        solve_error: bool,
        repair_changes: Option<Vec<FileChange>>,
        repair_error: bool,
        challenge_findings: Vec<ChallengeFinding>,
        /// When set, the challenger returns findings on the first call and
        /// an empty list afterwards — the one-repair-then-clean path.
        challenge_once: bool,
        challenge_error: bool,
        challenge_calls: StdMutex<u32>,
        // Spies proving real context crossed the model boundary.
        solve_saw_paths: StdMutex<Vec<String>>,
        solve_read_lib: StdMutex<Option<String>>,
        challenge_saw_diff: StdMutex<String>,
        repair_saw_checks: StdMutex<Vec<String>>,
    }

    impl FakeEnv {
        fn new() -> Self {
            Self {
                approve: true,
                submit_called: StdMutex::new(false),
                manifest: Some(MANIFEST),
                label: None,
                attested_sha: None,
                snapshot_files: default_snapshot(),
                skipped: vec![],
                truncated: false,
                reproduce_argv: vec![],
                solve_changes: None,
                solve_error: false,
                repair_changes: None,
                repair_error: false,
                challenge_findings: vec![],
                challenge_once: false,
                challenge_error: false,
                challenge_calls: StdMutex::new(0),
                solve_saw_paths: StdMutex::new(vec![]),
                solve_read_lib: StdMutex::new(None),
                challenge_saw_diff: StdMutex::new(String::new()),
                repair_saw_checks: StdMutex::new(vec![]),
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
                labels: self.label.iter().map(|l| l.to_string()).collect(),
                maintainer_comments: vec![],
            })
        }

        async fn fetch_repository(&self, _repo: &str) -> Result<Repository> {
            Ok(repository())
        }

        async fn attest_base_sha(&self, _repo: &str, _issue: Option<i64>) -> Result<String> {
            Ok(self.attested_sha.clone().unwrap_or_else(|| SHA.to_string()))
        }

        async fn fetch_base_snapshot(&self, _repo: &str, _base_sha: &str) -> Result<BaseSnapshot> {
            Ok(BaseSnapshot {
                files: self.snapshot_files.clone(),
                skipped: self.skipped.clone(),
                truncated: self.truncated,
            })
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
            _workspace: &WorkspaceView<'_>,
        ) -> Result<Vec<Vec<String>>> {
            Ok(self.reproduce_argv.clone())
        }

        async fn solve(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            workspace: &WorkspaceView<'_>,
        ) -> Result<SolverCandidate> {
            *self.solve_saw_paths.lock().unwrap() = workspace.list_paths();
            let before = workspace
                .read_text("src/lib.rs", crate::exec::workspace::VIEW_READ_LIMIT)
                .map(|t| t.content);
            *self.solve_read_lib.lock().unwrap() = before.clone();
            if self.solve_error {
                return Err(ContribError::Config("solver transport failed".into()));
            }
            let changes = self.solve_changes.clone().unwrap_or_else(|| {
                let before = before.unwrap_or_default();
                vec![FileChange {
                    path: "src/lib.rs".into(),
                    original_content: Some(before.clone()),
                    new_content: format!("{before}pub fn b() {{}}\n"),
                    is_new_file: false,
                    is_deleted: false,
                }]
            });
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
                changes,
                tests_added: vec![],
            })
        }

        async fn repair(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            request: &RepairRequest<'_>,
        ) -> Result<Vec<FileChange>> {
            *self.repair_saw_checks.lock().unwrap() = request
                .failing_checks
                .iter()
                .map(|c| c.name.clone())
                .collect();
            if self.repair_error {
                return Err(ContribError::Config("repair transport failed".into()));
            }
            if let Some(changes) = &self.repair_changes {
                return Ok(changes.clone());
            }
            // Quote the CURRENT file as the claimed preimage — the repair
            // contract accepts base or current content.
            let current = request
                .workspace
                .read_text("src/lib.rs", crate::exec::workspace::VIEW_READ_LIMIT)
                .map(|t| t.content)
                .unwrap_or_else(|| "pub fn a() {}\n".into());
            Ok(vec![FileChange {
                path: "src/lib.rs".into(),
                original_content: Some(current.clone()),
                new_content: format!("{current}pub fn c() {{}}\n"),
                is_new_file: false,
                is_deleted: false,
            }])
        }

        async fn challenge(
            &self,
            _repo: &str,
            _spec: &TaskSpec,
            input: &ChallengeInput<'_>,
        ) -> Result<Vec<ChallengeFinding>> {
            *self.challenge_saw_diff.lock().unwrap() = input.diff.text.clone();
            *self.challenge_calls.lock().unwrap() += 1;
            if self.challenge_error {
                return Err(ContribError::Config("challenger transport failed".into()));
            }
            if self.challenge_once && *self.challenge_calls.lock().unwrap() > 1 {
                return Ok(vec![]);
            }
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
            contribution: &Contribution,
            repo: &Repository,
            evidence: &EvidenceCapsuleV3,
            run: &ContributionRun,
            permit: &ContributionPermit,
        ) -> Result<PrResult> {
            // Mirror the production write path: the capsule is re-validated
            // against the exact candidate before any "external" write.
            if let Err(violations) =
                evidence.validate_for_submission(contribution, repo, run, permit, Utc::now())
            {
                return Err(ContribError::Config(format!(
                    "evidence validation failed: {violations:?}"
                )));
            }
            *self.submit_called.lock().unwrap() = true;
            Ok(PrResult {
                repo: repo.clone(),
                contribution: contribution.clone(),
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
        let env = FakeEnv {
            manifest: None,
            label: Some("contribai-approved"),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.authorize(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Authorized);
        assert!(run.permit_id.is_some());
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
        let env = FakeEnv {
            attested_sha: Some("f".repeat(40)),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.authorize(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    // ── Context reaches the model boundary ────────────────────────────

    #[tokio::test]
    async fn solver_sees_real_workspace_context() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Approved);
        // The solver read the real file contents through the bounded view.
        assert_eq!(
            env.solve_read_lib.lock().unwrap().as_deref(),
            Some("pub fn a() {}\n")
        );
        assert!(env
            .solve_saw_paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "src/lib.rs"));
    }

    #[tokio::test]
    async fn challenger_sees_actual_unified_diff() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Approved);
        let diff = env.challenge_saw_diff.lock().unwrap().clone();
        assert!(diff.contains("--- a/src/lib.rs"), "diff:\n{diff}");
        assert!(diff.contains("+++ b/src/lib.rs"), "diff:\n{diff}");
        assert!(diff.contains("+pub fn b() {}"), "diff:\n{diff}");
    }

    // ── Materialization completeness ──────────────────────────────────

    #[tokio::test]
    async fn in_scope_skipped_file_blocks_prepare() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            skipped: vec![SkippedFile {
                path: "src/big.rs".into(),
                reason: SkipReason::Oversized,
                size: 200_000,
            }],
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        // `prepare` transitions to Blocked and reports the gap as an error.
        assert!(exec.drive(&run.run_id).await.is_err());
        let run = memory.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Blocked);
        let artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        let report = artifacts.materialization.unwrap();
        assert!(!report.is_complete());
        assert!(report.gaps.iter().any(|g| g.contains("src/big.rs")));
    }

    #[tokio::test]
    async fn truncated_snapshot_blocks_prepare() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            truncated: true,
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        assert!(exec.drive(&run.run_id).await.is_err());
        let run = memory.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    // ── Candidate binding ─────────────────────────────────────────────

    fn change(path: &str, original: Option<&str>, new: &str) -> FileChange {
        FileChange {
            path: path.into(),
            original_content: original.map(str::to_string),
            new_content: new.into(),
            is_new_file: false,
            is_deleted: false,
        }
    }

    #[tokio::test]
    async fn solver_claimed_preimage_mismatch_blocks() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            solve_changes: Some(vec![change(
                "src/lib.rs",
                Some("hallucinated base content\n"),
                "pub fn z() {}\n",
            )]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    #[tokio::test]
    async fn solver_out_of_scope_path_blocks() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            solve_changes: Some(vec![change("docs/readme.md", None, "x\n")]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    #[tokio::test]
    async fn solver_protected_path_blocks_even_when_scope_allows() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            manifest: Some(MANIFEST_ALL),
            solve_changes: Some(vec![change(".github/workflows/ci.yml", None, "x\n")]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    #[tokio::test]
    async fn solver_unsafe_path_blocks() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            solve_changes: Some(vec![change("../escape.rs", None, "x\n")]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    #[tokio::test]
    async fn solver_deletion_is_rejected() {
        let (dir, memory) = setup();
        let mut deleted = change("src/lib.rs", Some("pub fn a() {}\n"), "");
        deleted.is_deleted = true;
        let env = FakeEnv {
            solve_changes: Some(vec![deleted]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    #[tokio::test]
    async fn oversized_candidate_blocks() {
        let (dir, memory) = setup();
        let big = std::iter::once("pub fn a() {}\n".to_string())
            .chain((0..300).map(|i| format!("pub fn f{i}() {{}}\n")))
            .collect::<String>();
        let env = FakeEnv {
            solve_changes: Some(vec![change("src/lib.rs", Some("pub fn a() {}\n"), &big)]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    // ── Fail-closed model failures ────────────────────────────────────

    #[tokio::test]
    async fn solver_transport_failure_fails_run() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            solve_error: true,
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let err = exec.drive(&run.run_id).await.unwrap_err();
        assert!(err.to_string().contains("solver"));
        let run = memory.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn challenger_failure_blocks_with_recorded_report() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            challenge_error: true,
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
        let artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        let report = artifacts.challenge.expect("not-run report recorded");
        assert!(report.findings.is_empty());
    }

    #[tokio::test]
    async fn missing_required_check_blocks_and_repair_sees_it() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            manifest: Some(MANIFEST_STRICT),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        // cargo test cannot run in a bare snapshot → required check skipped
        // → incomplete → bounded repair → exhausted → blocked.
        assert_eq!(run.state, RunState::Blocked);
        let checks = env.repair_saw_checks.lock().unwrap().clone();
        assert!(checks.iter().any(|c| c == "cargo_test"), "{checks:?}");
    }

    #[tokio::test]
    async fn repair_then_clean_challenge_approves_and_rebinds() {
        let (dir, memory) = setup();
        let env = FakeEnv {
            challenge_once: true,
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
        assert_eq!(run.state, RunState::Approved);
        assert_eq!(run.repair_iterations, 1);
        // Review fingerprint binds to the POST-repair candidate.
        let artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        let capsule = artifacts.capsule.unwrap();
        assert_eq!(capsule.review_fingerprint, run.review_fingerprint);
        assert!(capsule
            .materialization
            .as_ref()
            .map(|m| m.is_complete())
            .unwrap_or(false));
    }

    // ── Consent / boundary retests ────────────────────────────────────

    #[tokio::test]
    async fn expired_run_fails_closed() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run =
            ContributionRun::new("octo/repo", Some(7), SHA, Utc::now() - Duration::seconds(5));
        memory.insert_run(&run).unwrap();
        let exec = executor(&memory, &env, dir.path(), false);
        assert!(exec.drive(&run.run_id).await.is_err());
        let run = memory.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Expired);
    }

    #[tokio::test]
    async fn candidate_change_after_approval_fails_submission() {
        let (dir, memory) = setup();
        let env = FakeEnv::new();
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), true);
        // Drive to evidence (no review), then approve explicitly.
        let run = exec.drive_to_evidence(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::ReadyForReview);
        let run = exec.review(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Approved);
        // Tamper: the candidate changes after the human approved it.
        let mut artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        artifacts
            .contribution
            .as_mut()
            .unwrap()
            .changes
            .push(change("src/extra.rs", None, "sneaky\n"));
        artifacts.save(dir.path(), &run.run_id).unwrap();
        let err = exec.submit(&run.run_id).await.unwrap_err();
        assert!(err.to_string().contains("evidence validation"));
        assert!(!*env.submit_called.lock().unwrap());
        let run = memory.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Approved); // not submitted
    }

    // ── Two-leg reproduction with a real command ──────────────────────

    fn python_available() -> bool {
        std::process::Command::new("python")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn two_leg_reproduction_with_real_command() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let (dir, memory) = setup();
        let env = FakeEnv {
            snapshot_files: vec![
                ("src/lib.rs".into(), b"pub fn a() {}\n".to_vec()),
                (
                    "check_fix.py".into(),
                    b"import os, sys\nsys.exit(0 if os.path.exists('src/fixed.txt') else 1)\n"
                        .to_vec(),
                ),
            ],
            reproduce_argv: vec![vec!["python".into(), "check_fix.py".into()]],
            solve_changes: Some(vec![change("src/fixed.txt", None, "fixed\n")]),
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Approved);
        let artifacts = RunArtifacts::load(dir.path(), &run.run_id).unwrap();
        let repro = artifacts.reproduction.unwrap();
        assert!(repro.attempted && repro.reproduced);
        let candidate = repro.candidate.expect("candidate leg recorded");
        assert!(candidate.resolved);
        assert_eq!(
            repro.commands,
            vec![vec!["python".to_string(), "check_fix.py".to_string()]]
        );
    }

    #[tokio::test]
    async fn unresolved_candidate_reproduction_blocks() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let (dir, memory) = setup();
        let env = FakeEnv {
            snapshot_files: vec![
                ("src/lib.rs".into(), b"pub fn a() {}\n".to_vec()),
                (
                    "check_fix.py".into(),
                    b"import os, sys\nsys.exit(0 if os.path.exists('src/fixed.txt') else 1)\n"
                        .to_vec(),
                ),
            ],
            reproduce_argv: vec![vec!["python".into(), "check_fix.py".into()]],
            // The solver changes lib.rs but never creates the marker —
            // the reproduced behavior persists on the candidate.
            ..FakeEnv::new()
        };
        let run = new_run(&memory);
        let exec = executor(&memory, &env, dir.path(), false);
        let run = exec.drive(&run.run_id).await.unwrap();
        assert_eq!(run.state, RunState::Blocked);
    }

    // ── Strict model-output parsing (unit) ────────────────────────────

    #[test]
    fn solver_output_parsing_fails_closed() {
        let spec = TaskSpec::draft("o/r", Some(1), "t");
        assert!(parse_solver_output("not json", &spec).is_err());
        assert!(parse_solver_output("{\"changes\": [", &spec).is_err());
        assert!(parse_solver_output("{\"changes\": []}", &spec).is_err());
        assert!(parse_solver_output("{\"title\": \"x\"}", &spec).is_err());
        assert!(parse_solver_output("{\"changes\": [{\"path\": \"a.rs\"}]}", &spec).is_err());
        assert!(parse_solver_output(
            "{\"changes\": [{\"path\": \"a.rs\", \"new_content\": \"\"}]}",
            &spec
        )
        .is_err());
        // Prose-wrapped JSON parses; the extracted document is complete.
        let ok = parse_solver_output(
            "Here is the patch:\n{\"changes\": [{\"path\": \"a.rs\", \"new_content\": \"x\"}]}\nDone.",
            &spec,
        );
        assert!(ok.is_ok());
        // Fenced JSON parses too.
        let fenced = parse_solver_output(
            "```json\n{\"changes\": [{\"path\": \"a.rs\", \"new_content\": \"x\"}]}\n```",
            &spec,
        );
        assert!(fenced.is_ok());
    }

    #[test]
    fn findings_output_parsing_fails_closed() {
        assert!(parse_findings_output("not json").is_err());
        assert!(parse_findings_output("{\"findings\": \"oops\"}").is_err());
        assert!(parse_findings_output("{\"findings\": [{}]}").is_err());
        assert!(parse_findings_output("{\"findings\": []}")
            .unwrap()
            .is_empty());
        let ok = parse_findings_output(
            "{\"findings\": [{\"severity\": \"high\", \"category\": \"security\", \"summary\": \"bad\"}]}",
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(
            ok[0].severity,
            crate::core::challenge::ChallengeSeverity::High
        );
    }
}
