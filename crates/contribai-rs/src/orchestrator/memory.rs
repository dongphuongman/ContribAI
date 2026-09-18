//! Persistent memory system using SQLite.
//!
//! Port from Python `orchestrator/memory.py`.
//! Tracks analyzed repos, submitted PRs, outcome learning,
//! and working memory with TTL.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::info;

use crate::core::admission::{AdmissionAuditRecord, AdmissionAuditVerification, ConsentSource};
use crate::core::error::{ContribError, Result};
use crate::core::run::{ContributionRun, RunEvent, RunState};

/// Parse an RFC-3339 timestamp stored in the database.
fn parse_db_time(value: &str) -> Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| ContribError::Database(format!("bad timestamp {value:?}: {e}")))
}

/// A single message in a PR conversation thread.
pub struct ConversationMessage {
    pub repo: String,
    pub pr_number: i64,
    /// "maintainer", "contribai", or "bot"
    pub role: String,
    pub author: String,
    pub body: String,
    pub comment_id: i64,
    pub is_inline: bool,
    pub file_path: Option<String>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS analyzed_repos (
    full_name   TEXT PRIMARY KEY,
    language    TEXT,
    stars       INTEGER,
    analyzed_at TEXT,
    findings    INTEGER DEFAULT 0,
    metadata    TEXT DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS submitted_prs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    repo        TEXT NOT NULL,
    pr_number   INTEGER NOT NULL,
    pr_url      TEXT NOT NULL,
    title       TEXT NOT NULL,
    type        TEXT NOT NULL,
    status      TEXT DEFAULT 'open',
    branch      TEXT,
    fork        TEXT,
    created_at  TEXT,
    updated_at  TEXT,
    UNIQUE(repo, pr_number)
);

CREATE TABLE IF NOT EXISTS findings_cache (
    id          TEXT PRIMARY KEY,
    repo        TEXT NOT NULL,
    type        TEXT NOT NULL,
    severity    TEXT NOT NULL,
    title       TEXT NOT NULL,
    file_path   TEXT,
    status      TEXT DEFAULT 'new',
    created_at  TEXT
);

CREATE TABLE IF NOT EXISTS run_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TEXT,
    finished_at TEXT,
    repos_analyzed INTEGER DEFAULT 0,
    prs_created  INTEGER DEFAULT 0,
    findings     INTEGER DEFAULT 0,
    errors       INTEGER DEFAULT 0,
    metadata     TEXT DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS pr_outcomes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    repo        TEXT NOT NULL,
    pr_number   INTEGER NOT NULL,
    pr_url      TEXT NOT NULL,
    pr_type     TEXT NOT NULL,
    outcome     TEXT NOT NULL,
    feedback    TEXT DEFAULT '',
    time_to_close_hours REAL DEFAULT 0,
    recorded_at TEXT,
    UNIQUE(repo, pr_number)
);

CREATE TABLE IF NOT EXISTS repo_preferences (
    repo        TEXT PRIMARY KEY,
    preferred_types TEXT DEFAULT '[]',
    rejected_types  TEXT DEFAULT '[]',
    merge_rate  REAL DEFAULT 0.0,
    avg_review_hours REAL DEFAULT 0.0,
    notes       TEXT DEFAULT '',
    updated_at  TEXT
);

CREATE TABLE IF NOT EXISTS working_memory (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    repo        TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    language    TEXT DEFAULT '',
    created_at  TEXT,
    expires_at  TEXT,
    UNIQUE(repo, key)
);

CREATE TABLE IF NOT EXISTS dream_meta (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT
);

CREATE TABLE IF NOT EXISTS pr_conversations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    repo        TEXT NOT NULL,
    pr_number   INTEGER NOT NULL,
    role        TEXT NOT NULL,
    author      TEXT NOT NULL,
    body        TEXT NOT NULL,
    comment_id  INTEGER DEFAULT 0,
    is_inline   INTEGER DEFAULT 0,
    file_path   TEXT,
    created_at  TEXT,
    UNIQUE(repo, pr_number, comment_id)
);

CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    mode        TEXT NOT NULL DEFAULT 'build',
    status      TEXT NOT NULL DEFAULT 'running',
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS admission_audit (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    receipt          TEXT NOT NULL UNIQUE,
    previous_receipt TEXT,
    repository       TEXT NOT NULL,
    decision         TEXT NOT NULL,
    stage            TEXT NOT NULL,
    recorded_at      TEXT NOT NULL,
    payload          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS contribution_runs (
    run_id                TEXT PRIMARY KEY,
    repository            TEXT NOT NULL,
    issue                 INTEGER,
    base_sha              TEXT NOT NULL,
    permit_id             TEXT,
    consent_source        TEXT,
    task_fingerprint      TEXT,
    candidate_fingerprint TEXT,
    review_fingerprint    TEXT,
    review_decided_at     TEXT,
    state                 TEXT NOT NULL,
    repair_iterations     INTEGER DEFAULT 0,
    reproduction          TEXT,
    challenge_summary     TEXT,
    draft_pr_number       INTEGER,
    draft_pr_url          TEXT,
    terminal_reason       TEXT,
    solver_model          TEXT,
    challenger_model      TEXT,
    policy_version        INTEGER DEFAULT 1,
    planner_version       INTEGER DEFAULT 1,
    validator_version     INTEGER DEFAULT 1,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    expires_at            TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS run_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    state_from  TEXT NOT NULL,
    state_to    TEXT NOT NULL,
    event       TEXT NOT NULL,
    detail      TEXT DEFAULT '',
    recorded_at TEXT NOT NULL
);

-- Indexes for hot query paths
CREATE INDEX IF NOT EXISTS idx_submitted_prs_created_at ON submitted_prs(created_at);
CREATE INDEX IF NOT EXISTS idx_submitted_prs_status ON submitted_prs(status);
CREATE INDEX IF NOT EXISTS idx_pr_conversations_repo_pr ON pr_conversations(repo, pr_number);
CREATE INDEX IF NOT EXISTS idx_working_memory_repo_key ON working_memory(repo, key);
CREATE INDEX IF NOT EXISTS idx_working_memory_expires ON working_memory(expires_at);
CREATE INDEX IF NOT EXISTS idx_pr_outcomes_repo ON pr_outcomes(repo);
CREATE INDEX IF NOT EXISTS idx_findings_cache_repo ON findings_cache(repo);
CREATE INDEX IF NOT EXISTS idx_admission_audit_recorded_at ON admission_audit(recorded_at);
CREATE INDEX IF NOT EXISTS idx_admission_audit_repo ON admission_audit(repository);
CREATE INDEX IF NOT EXISTS idx_admission_audit_decision ON admission_audit(decision);
CREATE INDEX IF NOT EXISTS idx_contribution_runs_repo ON contribution_runs(repository);
CREATE INDEX IF NOT EXISTS idx_contribution_runs_state ON contribution_runs(state);
CREATE INDEX IF NOT EXISTS idx_run_events_run ON run_events(run_id);
"#;

/// Persistent memory backed by SQLite.
pub struct Memory {
    db: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl Memory {
    /// Safely lock the DB mutex, recovering from poisoned state.
    fn lock_db(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|e| ContribError::Config(format!("DB lock poisoned: {}", e)))
    }

    /// Open (or create) a SQLite database.
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ContribError::Config(format!("Cannot create db dir: {}", e)))?;
        }

        let conn = Connection::open(db_path)
            .map_err(|e| ContribError::Config(format!("SQLite open error: {}", e)))?;

        // Enable WAL for concurrency
        conn.execute_batch("PRAGMA journal_mode=WAL;").ok();

        // Create schema
        conn.execute_batch(SCHEMA)
            .map_err(|e| ContribError::Config(format!("Schema init error: {}", e)))?;

        info!(path = ?db_path, "Memory initialized");
        Ok(Self {
            db: Mutex::new(conn),
            db_path: db_path.to_path_buf(),
        })
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| ContribError::Config(format!("SQLite error: {}", e)))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| ContribError::Config(format!("Schema error: {}", e)))?;
        Ok(Self {
            db: Mutex::new(conn),
            db_path: PathBuf::from(":memory:"),
        })
    }

    // ── Repos ──────────────────────────────────────────────────────────────

    /// Check if a repo has been analyzed before (no time limit).
    pub fn has_analyzed(&self, full_name: &str) -> Result<bool> {
        let db = self.lock_db()?;
        let exists: bool = db
            .query_row(
                "SELECT 1 FROM analyzed_repos WHERE full_name = ?1",
                params![full_name],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?
            .unwrap_or(false);
        Ok(exists)
    }

    /// Check if a repo was analyzed within the last `days` days.
    /// Returns false if analyzed longer ago (allows re-analysis).
    pub fn has_analyzed_since(&self, full_name: &str, days: i64) -> Result<bool> {
        let db = self.lock_db()?;
        let exists: bool = db
            .query_row(
                "SELECT 1 FROM analyzed_repos WHERE full_name = ?1
                 AND analyzed_at > datetime('now', ?2)",
                params![full_name, format!("-{} days", days)],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?
            .unwrap_or(false);
        Ok(exists)
    }

    /// Record that a repo was analyzed.
    pub fn record_analysis(
        &self,
        full_name: &str,
        language: &str,
        stars: i64,
        findings_count: i64,
    ) -> Result<()> {
        let db = self.lock_db()?;
        db.execute(
            "INSERT OR REPLACE INTO analyzed_repos
             (full_name, language, stars, analyzed_at, findings)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                full_name,
                language,
                stars,
                Utc::now().to_rfc3339(),
                findings_count
            ],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(())
    }

    // ── PRs ────────────────────────────────────────────────────────────────

    /// Record a submitted PR.
    #[allow(clippy::too_many_arguments)]
    pub fn record_pr(
        &self,
        repo: &str,
        pr_number: i64,
        pr_url: &str,
        title: &str,
        pr_type: &str,
        branch: &str,
        fork: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let db = self.lock_db()?;
        db.execute(
            "INSERT OR REPLACE INTO submitted_prs
             (repo, pr_number, pr_url, title, type, branch, fork, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![repo, pr_number, pr_url, title, pr_type, branch, fork, now, now],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(())
    }

    /// Update PR status.
    pub fn update_pr_status(&self, repo: &str, pr_number: i64, status: &str) -> Result<()> {
        let db = self.lock_db()?;
        db.execute(
            "UPDATE submitted_prs SET status = ?1, updated_at = ?2
             WHERE repo = ?3 AND pr_number = ?4",
            params![status, Utc::now().to_rfc3339(), repo, pr_number],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(())
    }

    /// Get PRs, optionally filtered by status.
    pub fn get_prs(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HashMap<String, String>>> {
        let db = self.lock_db()?;
        let mut rows = Vec::new();

        if let Some(s) = status {
            let mut stmt = db
                .prepare(
                    "SELECT repo, pr_number, pr_url, title, type, status, branch, fork, created_at
                     FROM submitted_prs WHERE status = ?1
                     ORDER BY created_at DESC LIMIT ?2",
                )
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            let mapped = stmt
                .query_map(params![s, limit as i64], |row| Ok(pr_row_to_map(row)))
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            for m in mapped.flatten() {
                rows.push(m);
            }
        } else {
            let mut stmt = db
                .prepare(
                    "SELECT repo, pr_number, pr_url, title, type, status, branch, fork, created_at
                     FROM submitted_prs ORDER BY created_at DESC LIMIT ?1",
                )
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            let mapped = stmt
                .query_map(params![limit as i64], |row| Ok(pr_row_to_map(row)))
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            for m in mapped.flatten() {
                rows.push(m);
            }
        }

        Ok(rows)
    }

    /// Get number of PRs created today.
    pub fn get_today_pr_count(&self) -> Result<usize> {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let db = self.lock_db()?;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM submitted_prs WHERE created_at LIKE ?1",
                params![format!("{}%", today)],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(count as usize)
    }

    // ── Run Log ────────────────────────────────────────────────────────────

    /// Record the start of a pipeline run.
    pub fn start_run(&self) -> Result<i64> {
        let db = self.lock_db()?;
        db.execute(
            "INSERT INTO run_log (started_at) VALUES (?1)",
            params![Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(db.last_insert_rowid())
    }

    /// Record completion of a pipeline run.
    pub fn finish_run(
        &self,
        run_id: i64,
        repos_analyzed: i64,
        prs_created: i64,
        findings: i64,
        errors: i64,
    ) -> Result<()> {
        let db = self.lock_db()?;
        db.execute(
            "UPDATE run_log SET finished_at = ?1, repos_analyzed = ?2,
             prs_created = ?3, findings = ?4, errors = ?5 WHERE id = ?6",
            params![
                Utc::now().to_rfc3339(),
                repos_analyzed,
                prs_created,
                findings,
                errors,
                run_id
            ],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(())
    }

    /// Get overall statistics.
    pub fn get_stats(&self) -> Result<HashMap<String, i64>> {
        let db = self.lock_db()?;
        let mut stats = HashMap::new();

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM analyzed_repos", [], |r| r.get(0))
            .unwrap_or(0);
        stats.insert("total_repos_analyzed".into(), count);

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM submitted_prs", [], |r| r.get(0))
            .unwrap_or(0);
        stats.insert("total_prs_submitted".into(), count);

        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM submitted_prs WHERE status = 'merged'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        stats.insert("prs_merged".into(), count);

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM run_log", [], |r| r.get(0))
            .unwrap_or(0);
        stats.insert("total_runs".into(), count);

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM admission_audit", [], |r| r.get(0))
            .unwrap_or(0);
        stats.insert("admission_decisions_total".into(), count);

        for decision in ["approved", "blocked", "rejected", "skipped", "error"] {
            let count: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM admission_audit WHERE decision = ?1",
                    params![decision],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            stats.insert(format!("admission_{decision}_total"), count);
        }

        Ok(stats)
    }

    // ── Admission audit ──────────────────────────────────────────────────

    /// Verify the retained ledger and append a terminal decision in one write transaction.
    pub fn record_admission_audit(
        &self,
        record: AdmissionAuditRecord,
    ) -> Result<AdmissionAuditRecord> {
        let mut db = self.lock_db()?;
        let transaction = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| ContribError::Database(format!("Admission audit begin: {error}")))?;
        if !Self::verify_admission_audit_chain_in(&transaction)?.valid {
            return Err(ContribError::Database(
                "Admission audit integrity check failed; preserve the database and follow docs/AUDIT_RECOVERY.md before submitting".into(),
            ));
        }
        let previous_receipt = transaction
            .query_row(
                "SELECT receipt FROM admission_audit ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| {
                ContribError::Database(format!("Admission audit predecessor query: {error}"))
            })?;
        let sealed = record.seal(previous_receipt).map_err(|error| {
            ContribError::Database(format!("Admission audit receipt encoding: {error}"))
        })?;
        if !sealed.verify_receipt() {
            return Err(ContribError::Database(
                "Admission audit record uses an unsupported schema".into(),
            ));
        }
        let payload = serde_json::to_string(&sealed).map_err(|error| {
            ContribError::Database(format!("Admission audit payload encoding: {error}"))
        })?;
        transaction
            .execute(
                "INSERT INTO admission_audit
                 (receipt, previous_receipt, repository, decision, stage, recorded_at, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    sealed.receipt,
                    sealed.previous_receipt,
                    sealed.repository,
                    sealed.decision.as_str(),
                    sealed.stage.as_str(),
                    sealed.recorded_at.to_rfc3339(),
                    payload,
                ],
            )
            .map_err(|error| ContribError::Database(format!("Admission audit insert: {error}")))?;
        transaction
            .commit()
            .map_err(|error| ContribError::Database(format!("Admission audit commit: {error}")))?;
        Ok(sealed)
    }

    /// Read recent admission decisions without exposing generated file contents.
    pub fn get_admission_audits(
        &self,
        repository: Option<&str>,
        decision: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AdmissionAuditRecord>> {
        let db = self.lock_db()?;
        let mut statement = db
            .prepare(
                "SELECT payload FROM admission_audit
                 WHERE (?1 IS NULL OR repository = ?1)
                   AND (?2 IS NULL OR decision = ?2)
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(|error| ContribError::Database(format!("Admission audit query: {error}")))?;
        let payloads = statement
            .query_map(
                params![repository, decision, limit.clamp(1, 1000) as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| {
                ContribError::Database(format!("Admission audit query map: {error}"))
            })?;
        let mut records = Vec::new();
        for payload in payloads {
            let payload = payload
                .map_err(|error| ContribError::Database(format!("Admission audit row: {error}")))?;
            let record = serde_json::from_str(&payload)
                .map_err(|_| ContribError::Database("Admission audit payload is invalid".into()))?;
            records.push(record);
        }
        Ok(records)
    }

    /// Verify receipt hashes and predecessor links for the complete local ledger.
    pub fn verify_admission_audit_chain(&self) -> Result<AdmissionAuditVerification> {
        let db = self.lock_db()?;
        Self::verify_admission_audit_chain_in(&db)
    }

    fn verify_admission_audit_chain_in(db: &Connection) -> Result<AdmissionAuditVerification> {
        let mut statement = db
            .prepare(
                "SELECT payload, receipt, previous_receipt, repository, decision, stage, recorded_at
                 FROM admission_audit ORDER BY id ASC",
            )
            .map_err(|error| {
                ContribError::Database(format!("Admission audit verification query: {error}"))
            })?;
        let payloads = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|error| {
                ContribError::Database(format!("Admission audit verification map: {error}"))
            })?;
        let mut expected_previous: Option<String> = None;
        let mut records_checked = 0;
        for stored in payloads {
            let (payload, receipt, previous_receipt, repository, decision, stage, recorded_at) =
                stored.map_err(|error| {
                    ContribError::Database(format!("Admission audit verification row: {error}"))
                })?;
            let record: AdmissionAuditRecord = serde_json::from_str(&payload).map_err(|_| {
                ContribError::Database("Admission audit verification payload is invalid".into())
            })?;
            records_checked += 1;
            let columns_match = record.receipt == receipt
                && record.previous_receipt == previous_receipt
                && record.repository == repository
                && record.decision.as_str() == decision
                && record.stage.as_str() == stage
                && record.recorded_at.to_rfc3339() == recorded_at;
            if record.previous_receipt != expected_previous
                || !columns_match
                || !record.verify_receipt()
            {
                return Ok(AdmissionAuditVerification {
                    valid: false,
                    records_checked,
                    first_invalid_receipt: Some(receipt),
                });
            }
            expected_previous = Some(record.receipt);
        }
        Ok(AdmissionAuditVerification {
            valid: true,
            records_checked,
            first_invalid_receipt: None,
        })
    }

    // ── Contribution runs ─────────────────────────────────────────────

    /// Persist a newly created run. `run_id` is unique; a duplicate insert
    /// fails rather than silently merging two runs.
    pub fn insert_run(&self, run: &ContributionRun) -> Result<()> {
        let db = self.lock_db()?;
        db.execute(
            "INSERT INTO contribution_runs
             (run_id, repository, issue, base_sha, permit_id, consent_source,
              task_fingerprint, candidate_fingerprint, review_fingerprint,
              review_decided_at, state, repair_iterations, reproduction,
              challenge_summary, draft_pr_number, draft_pr_url, terminal_reason,
              solver_model, challenger_model, policy_version, planner_version,
              validator_version, created_at, updated_at, expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            params![
                run.run_id,
                run.repository,
                run.issue,
                run.base_sha,
                run.permit_id,
                run.consent_source
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| ContribError::Database(format!("run consent encoding: {e}")))?,
                run.task_fingerprint,
                run.candidate_fingerprint,
                run.review_fingerprint,
                run.review_decided_at.map(|t| t.to_rfc3339()),
                run.state.as_str(),
                run.repair_iterations,
                run.reproduction,
                run.challenge_summary,
                run.draft_pr_number,
                run.draft_pr_url,
                run.terminal_reason,
                run.solver_model,
                run.challenger_model,
                run.policy_version,
                run.planner_version,
                run.validator_version,
                run.created_at.to_rfc3339(),
                run.updated_at.to_rfc3339(),
                run.expires_at.to_rfc3339(),
            ],
        )
        .map_err(|e| ContribError::Database(format!("run insert: {e}")))?;
        Ok(())
    }

    /// Load a run by ID.
    pub fn get_run(&self, run_id: &str) -> Result<Option<ContributionRun>> {
        let db = self.lock_db()?;
        Self::load_run_in(&db, run_id)
    }

    /// List runs newest-first, optionally filtered by repository and state.
    pub fn list_runs(
        &self,
        repository: Option<&str>,
        state: Option<RunState>,
        limit: usize,
    ) -> Result<Vec<ContributionRun>> {
        let db = self.lock_db()?;
        let state_str = state.map(|s| s.as_str());
        let mut stmt = db
            .prepare(
                "SELECT run_id, repository, issue, base_sha, permit_id, consent_source,
                        task_fingerprint, candidate_fingerprint, review_fingerprint,
                        review_decided_at, state, repair_iterations, reproduction,
                        challenge_summary, draft_pr_number, draft_pr_url, terminal_reason,
                        solver_model, challenger_model, policy_version, planner_version,
                        validator_version, created_at, updated_at, expires_at
                 FROM contribution_runs
                 WHERE (?1 IS NULL OR repository = ?1)
                   AND (?2 IS NULL OR state = ?2)
                 ORDER BY created_at DESC LIMIT ?3",
            )
            .map_err(|e| ContribError::Database(format!("run list query: {e}")))?;
        let rows = stmt
            .query_map(
                params![repository, state_str, limit.clamp(1, 1000) as i64],
                Self::run_from_row,
            )
            .map_err(|e| ContribError::Database(format!("run list map: {e}")))?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row.map_err(|e| ContribError::Database(format!("run row: {e}")))?);
        }
        Ok(runs)
    }

    /// Runs left in a non-terminal state — crash-recovery candidates.
    pub fn list_interrupted_runs(&self) -> Result<Vec<ContributionRun>> {
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT run_id, repository, issue, base_sha, permit_id, consent_source,
                        task_fingerprint, candidate_fingerprint, review_fingerprint,
                        review_decided_at, state, repair_iterations, reproduction,
                        challenge_summary, draft_pr_number, draft_pr_url, terminal_reason,
                        solver_model, challenger_model, policy_version, planner_version,
                        validator_version, created_at, updated_at, expires_at
                 FROM contribution_runs
                 WHERE state NOT IN ('submitted','blocked','needs_authorization','failed','expired','cancelled')
                 ORDER BY created_at ASC",
            )
            .map_err(|e| ContribError::Database(format!("interrupted runs query: {e}")))?;
        let rows = stmt
            .query_map([], Self::run_from_row)
            .map_err(|e| ContribError::Database(format!("interrupted runs map: {e}")))?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row.map_err(|e| ContribError::Database(format!("run row: {e}")))?);
        }
        Ok(runs)
    }

    /// Persist a snapshot of run fields that changed outside a transition
    /// (fingerprints, reproduction, challenge summary, draft PR identity).
    pub fn update_run(&self, run: &ContributionRun) -> Result<()> {
        let db = self.lock_db()?;
        Self::update_run_in(&db, run)
    }

    /// Atomically validate and persist a lifecycle transition plus its event.
    ///
    /// The current state is re-read inside an immediate write transaction, so
    /// a stale in-memory copy cannot move a run concurrently modified by
    /// another operator or a recovered process. This is the only way state
    /// may change — never a raw UPDATE from outside.
    pub fn transition_run(
        &self,
        run_id: &str,
        to: RunState,
        event: &str,
        detail: &str,
    ) -> Result<(ContributionRun, RunEvent)> {
        let mut db = self.lock_db()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| ContribError::Database(format!("run transition begin: {e}")))?;
        let mut run = Self::load_run_in(&tx, run_id)?
            .ok_or_else(|| ContribError::Database(format!("run {run_id} does not exist")))?;
        let run_event = run
            .transition(to, event, detail, Utc::now())
            .map_err(|error| ContribError::Database(format!("run transition denied: {error}")))?;
        Self::update_run_in(&tx, &run)?;
        Self::insert_event_in(&tx, &run_event)?;
        tx.commit()
            .map_err(|e| ContribError::Database(format!("run transition commit: {e}")))?;
        Ok((run, run_event))
    }

    /// Ordered lifecycle events for a run.
    pub fn get_run_events(&self, run_id: &str) -> Result<Vec<RunEvent>> {
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT run_id, state_from, state_to, event, detail, recorded_at
                 FROM run_events WHERE run_id = ?1 ORDER BY id ASC",
            )
            .map_err(|e| ContribError::Database(format!("run events query: {e}")))?;
        let rows = stmt
            .query_map(params![run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| ContribError::Database(format!("run events map: {e}")))?;
        let mut events = Vec::new();
        for row in rows {
            let (run_id, from, to, event, detail, recorded_at) =
                row.map_err(|e| ContribError::Database(format!("run event row: {e}")))?;
            events.push(RunEvent {
                run_id,
                state_from: RunState::parse(&from)
                    .ok_or_else(|| ContribError::Database(format!("bad run state {from}")))?,
                state_to: RunState::parse(&to)
                    .ok_or_else(|| ContribError::Database(format!("bad run state {to}")))?,
                event,
                detail,
                recorded_at: parse_db_time(&recorded_at)?,
            });
        }
        Ok(events)
    }

    fn load_run_in(db: &Connection, run_id: &str) -> Result<Option<ContributionRun>> {
        db.query_row(
            "SELECT run_id, repository, issue, base_sha, permit_id, consent_source,
                    task_fingerprint, candidate_fingerprint, review_fingerprint,
                    review_decided_at, state, repair_iterations, reproduction,
                    challenge_summary, draft_pr_number, draft_pr_url, terminal_reason,
                    solver_model, challenger_model, policy_version, planner_version,
                    validator_version, created_at, updated_at, expires_at
             FROM contribution_runs WHERE run_id = ?1",
            params![run_id],
            Self::run_from_row,
        )
        .optional()
        .map_err(|e| ContribError::Database(format!("run load: {e}")))
    }

    fn update_run_in(db: &Connection, run: &ContributionRun) -> Result<()> {
        let updated = db
            .execute(
                "UPDATE contribution_runs SET
                    repository = ?2, issue = ?3, base_sha = ?4, permit_id = ?5,
                    consent_source = ?6, task_fingerprint = ?7,
                    candidate_fingerprint = ?8, review_fingerprint = ?9,
                    review_decided_at = ?10, state = ?11, repair_iterations = ?12,
                    reproduction = ?13, challenge_summary = ?14,
                    draft_pr_number = ?15, draft_pr_url = ?16, terminal_reason = ?17,
                    solver_model = ?18, challenger_model = ?19, policy_version = ?20,
                    planner_version = ?21, validator_version = ?22, created_at = ?23,
                    updated_at = ?24, expires_at = ?25
                 WHERE run_id = ?1",
                params![
                    run.run_id,
                    run.repository,
                    run.issue,
                    run.base_sha,
                    run.permit_id,
                    run.consent_source
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(|e| {
                            ContribError::Database(format!("run consent encoding: {e}"))
                        })?,
                    run.task_fingerprint,
                    run.candidate_fingerprint,
                    run.review_fingerprint,
                    run.review_decided_at.map(|t| t.to_rfc3339()),
                    run.state.as_str(),
                    run.repair_iterations,
                    run.reproduction,
                    run.challenge_summary,
                    run.draft_pr_number,
                    run.draft_pr_url,
                    run.terminal_reason,
                    run.solver_model,
                    run.challenger_model,
                    run.policy_version,
                    run.planner_version,
                    run.validator_version,
                    run.created_at.to_rfc3339(),
                    run.updated_at.to_rfc3339(),
                    run.expires_at.to_rfc3339(),
                ],
            )
            .map_err(|e| ContribError::Database(format!("run update: {e}")))?;
        if updated == 0 {
            return Err(ContribError::Database(format!(
                "run {} does not exist",
                run.run_id
            )));
        }
        Ok(())
    }

    fn insert_event_in(db: &Connection, event: &RunEvent) -> Result<()> {
        db.execute(
            "INSERT INTO run_events (run_id, state_from, state_to, event, detail, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.run_id,
                event.state_from.as_str(),
                event.state_to.as_str(),
                event.event,
                event.detail,
                event.recorded_at.to_rfc3339(),
            ],
        )
        .map_err(|e| ContribError::Database(format!("run event insert: {e}")))?;
        Ok(())
    }

    fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContributionRun> {
        let consent_source: Option<String> = row.get(5)?;
        let consent_source = consent_source
            .map(|raw| serde_json::from_str::<ConsentSource>(&raw))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        let state_raw: String = row.get(10)?;
        let state = RunState::parse(&state_raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                format!("unknown run state {state_raw}").into(),
            )
        })?;
        let text = |idx: usize| -> rusqlite::Result<Option<chrono::DateTime<Utc>>> {
            let raw: Option<String> = row.get(idx)?;
            raw.map(|v| {
                chrono::DateTime::parse_from_rfc3339(&v)
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            idx,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })
            })
            .transpose()
        };
        Ok(ContributionRun {
            run_id: row.get(0)?,
            repository: row.get(1)?,
            issue: row.get(2)?,
            base_sha: row.get(3)?,
            permit_id: row.get(4)?,
            consent_source,
            task_fingerprint: row.get(6)?,
            candidate_fingerprint: row.get(7)?,
            review_fingerprint: row.get(8)?,
            review_decided_at: text(9)?,
            state,
            repair_iterations: row.get(11)?,
            reproduction: row.get(12)?,
            challenge_summary: row.get(13)?,
            draft_pr_number: row.get(14)?,
            draft_pr_url: row.get(15)?,
            terminal_reason: row.get(16)?,
            solver_model: row.get(17)?,
            challenger_model: row.get(18)?,
            policy_version: row.get(19)?,
            planner_version: row.get(20)?,
            validator_version: row.get(21)?,
            created_at: text(22)?.unwrap_or_else(Utc::now),
            updated_at: text(23)?.unwrap_or_else(Utc::now),
            expires_at: text(24)?.unwrap_or_else(Utc::now),
        })
    }

    // ── Sessions ──────────────────────────────────────────────────────────

    /// Create a new session.
    pub fn create_session(&self, id: &str, name: &str, mode: &str) -> Result<()> {
        let db = self.lock_db()?;
        db.execute(
            "INSERT OR REPLACE INTO sessions (id, name, mode, status, created_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![id, name, mode, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Database(format!("Session create: {}", e)))?;
        Ok(())
    }

    /// Get all sessions.
    pub fn get_sessions(&self) -> Result<Vec<serde_json::Value>> {
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT id, name, mode, status, created_at FROM sessions ORDER BY created_at DESC",
            )
            .map_err(|e| ContribError::Database(format!("Session query: {}", e)))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "mode": r.get::<_, String>(2)?,
                    "status": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, String>(4)?,
                }))
            })
            .map_err(|e| ContribError::Database(format!("Session query map: {}", e)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ── Outcome Learning ──────────────────────────────────────────────────

    /// Record PR outcome (merged, closed, rejected).
    #[allow(clippy::too_many_arguments)]
    pub fn record_outcome(
        &self,
        repo: &str,
        pr_number: i64,
        pr_url: &str,
        pr_type: &str,
        outcome: &str,
        feedback: &str,
        time_to_close_hours: f64,
    ) -> Result<()> {
        {
            let db = self.lock_db()?;
            db.execute(
                "INSERT OR REPLACE INTO pr_outcomes
                 (repo, pr_number, pr_url, pr_type, outcome, feedback,
                  time_to_close_hours, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    repo,
                    pr_number,
                    pr_url,
                    pr_type,
                    outcome,
                    feedback,
                    time_to_close_hours,
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        }

        // Auto-update preferences
        self.update_repo_preferences(repo)?;
        Ok(())
    }

    /// Recompute repo preferences from outcome history.
    fn update_repo_preferences(&self, repo: &str) -> Result<()> {
        let db = self.lock_db()?;

        let mut stmt = db
            .prepare(
                "SELECT pr_type, outcome, time_to_close_hours FROM pr_outcomes WHERE repo = ?1",
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        let rows: Vec<(String, String, f64)> = stmt
            .query_map(params![repo], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2).unwrap_or(0.0),
                ))
            })
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok(());
        }

        let mut merged_types: Vec<String> = Vec::new();
        let mut rejected_types: Vec<String> = Vec::new();
        let mut total_hours = 0.0f64;
        let mut merged_count = 0usize;

        for (pr_type, outcome, hours) in &rows {
            if outcome == "merged" {
                if !merged_types.contains(pr_type) {
                    merged_types.push(pr_type.clone());
                }
                merged_count += 1;
                total_hours += hours;
            } else if (outcome == "closed" || outcome == "rejected")
                && !rejected_types.contains(pr_type)
            {
                rejected_types.push(pr_type.clone());
            }
        }

        let merge_rate = if !rows.is_empty() {
            merged_count as f64 / rows.len() as f64
        } else {
            0.0
        };
        let avg_hours = if merged_count > 0 {
            total_hours / merged_count as f64
        } else {
            0.0
        };

        db.execute(
            "INSERT OR REPLACE INTO repo_preferences
             (repo, preferred_types, rejected_types, merge_rate,
              avg_review_hours, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repo,
                serde_json::to_string(&merged_types).unwrap_or_default(),
                serde_json::to_string(&rejected_types).unwrap_or_default(),
                (merge_rate * 1000.0).round() / 1000.0,
                (avg_hours * 10.0).round() / 10.0,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        Ok(())
    }

    /// Get learned preferences for a specific repo.
    pub fn get_repo_preferences(&self, repo: &str) -> Result<Option<RepoPreferences>> {
        let db = self.lock_db()?;
        db.query_row(
            "SELECT preferred_types, rejected_types, merge_rate, avg_review_hours, notes
             FROM repo_preferences WHERE repo = ?1",
            params![repo],
            |row| {
                let pref: String = row.get(0)?;
                let rej: String = row.get(1)?;
                Ok(RepoPreferences {
                    preferred_types: serde_json::from_str(&pref).unwrap_or_default(),
                    rejected_types: serde_json::from_str(&rej).unwrap_or_default(),
                    merge_rate: row.get(2)?,
                    avg_review_hours: row.get(3)?,
                    notes: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))
    }

    // ── Working Memory ────────────────────────────────────────────────────

    /// Store hot context for a repo with TTL.
    pub fn store_context(
        &self,
        repo: &str,
        key: &str,
        value: &str,
        language: &str,
        ttl_hours: f64,
    ) -> Result<()> {
        let now = Utc::now();
        let expires = now + Duration::seconds((ttl_hours * 3600.0) as i64);
        let db = self.lock_db()?;
        db.execute(
            "INSERT OR REPLACE INTO working_memory
             (repo, key, value, language, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repo,
                key,
                value,
                language,
                now.to_rfc3339(),
                expires.to_rfc3339()
            ],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(())
    }

    /// Retrieve hot context, returns None if expired.
    pub fn get_context(&self, repo: &str, key: &str) -> Result<Option<String>> {
        let now = Utc::now().to_rfc3339();
        let db = self.lock_db()?;
        db.query_row(
            "SELECT value FROM working_memory
             WHERE repo = ?1 AND key = ?2 AND expires_at > ?3",
            params![repo, key, now],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))
    }

    /// Find context from repos with the same language.
    pub fn get_similar_context(
        &self,
        language: &str,
        key: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let now = Utc::now().to_rfc3339();
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT repo, value FROM working_memory
                 WHERE language = ?1 AND key = ?2 AND expires_at > ?3
                 ORDER BY created_at DESC LIMIT ?4",
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        let rows = stmt
            .query_map(params![language, key, now, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delete expired working memory entries.
    pub fn archive_expired(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let db = self.lock_db()?;
        let deleted = db
            .execute(
                "DELETE FROM working_memory WHERE expires_at <= ?1",
                params![now],
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
        Ok(deleted)
    }

    // ── Dream Memory Consolidation ────────────────────────────────────────

    /// Increment session counter for dream gating.
    pub fn increment_session_count(&self) -> Result<i64> {
        let db = self.lock_db()?;
        let current: i64 = db
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM dream_meta WHERE key = 'session_count'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let new_count = current + 1;
        db.execute(
            "INSERT OR REPLACE INTO dream_meta (key, value, updated_at)
             VALUES ('session_count', ?1, ?2)",
            params![new_count.to_string(), Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        Ok(new_count)
    }

    /// Check if dream consolidation should run (3-gate trigger).
    /// Gate 1: 24h since last dream
    /// Gate 2: At least 5 sessions since last dream
    /// Gate 3: No concurrent lock
    pub fn should_dream(&self) -> Result<bool> {
        let db = self.lock_db()?;

        // Gate 1: Time — 24h since last dream
        let last_dream: Option<String> = db
            .query_row(
                "SELECT value FROM dream_meta WHERE key = 'last_dream_at'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        if let Some(ts) = last_dream {
            if let Ok(last) = chrono::DateTime::parse_from_rfc3339(&ts) {
                let hours_since = (Utc::now() - last.with_timezone(&Utc)).num_hours();
                if hours_since < 24 {
                    return Ok(false);
                }
            }
        }

        // Gate 2: Sessions — at least 5 sessions
        let sessions: i64 = db
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM dream_meta WHERE key = 'session_count'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if sessions < 5 {
            return Ok(false);
        }

        // Gate 3: Lock — no concurrent dream
        let locked: Option<String> = db
            .query_row(
                "SELECT value FROM dream_meta WHERE key = 'dream_lock'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        if locked.as_deref() == Some("1") {
            return Ok(false);
        }

        Ok(true)
    }

    /// Run dream consolidation — aggregate PR outcomes into durable repo profiles.
    pub fn run_dream(&self) -> Result<DreamResult> {
        let db = self.lock_db()?;

        // Acquire lock
        db.execute(
            "INSERT OR REPLACE INTO dream_meta (key, value, updated_at)
             VALUES ('dream_lock', '1', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        let mut result = DreamResult::default();

        // Phase 1: Gather — get all repos with PR history
        let repos: Vec<String> = {
            let mut stmt = db
                .prepare("SELECT DISTINCT repo FROM pr_outcomes")
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
            let mapped = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
            mapped.filter_map(|r| r.ok()).collect()
        };

        // Phase 2: Consolidate — for each repo, compute profile
        for repo in &repos {
            let mut stmt = db
                .prepare(
                    "SELECT pr_type, outcome, time_to_close_hours, feedback
                     FROM pr_outcomes WHERE repo = ?1",
                )
                .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            let rows: Vec<(String, String, f64, String)> = {
                let mapped = stmt
                    .query_map(params![repo], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, f64>(2).unwrap_or(0.0),
                            row.get::<_, String>(3).unwrap_or_default(),
                        ))
                    })
                    .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;
                mapped.filter_map(|r| r.ok()).collect()
            };

            if rows.is_empty() {
                continue;
            }

            let mut type_stats: HashMap<String, (i32, i32)> = HashMap::new(); // (merged, total)
            let mut total_hours = 0.0f64;
            let mut merged_count = 0i32;
            let mut feedbacks: Vec<String> = Vec::new();

            for (pr_type, outcome, hours, feedback) in &rows {
                let entry = type_stats.entry(pr_type.clone()).or_insert((0, 0));
                entry.1 += 1;
                if outcome == "merged" {
                    entry.0 += 1;
                    merged_count += 1;
                    total_hours += hours;
                }
                if !feedback.is_empty() {
                    feedbacks.push(feedback.clone());
                }
            }

            let preferred: Vec<String> = type_stats
                .iter()
                .filter(|(_, (m, t))| *t > 0 && (*m as f64 / *t as f64) >= 0.5)
                .map(|(k, _)| k.clone())
                .collect();

            let avoid: Vec<String> = type_stats
                .iter()
                .filter(|(_, (m, t))| *t >= 2 && *m == 0)
                .map(|(k, _)| k.clone())
                .collect();

            let merge_rate = if !rows.is_empty() {
                merged_count as f64 / rows.len() as f64
            } else {
                0.0
            };

            let avg_hours = if merged_count > 0 {
                total_hours / merged_count as f64
            } else {
                0.0
            };

            // Summarize maintainer style from feedback
            let notes = if feedbacks.is_empty() {
                String::new()
            } else {
                format!(
                    "Last {} feedbacks recorded. Patterns: {}",
                    feedbacks.len(),
                    feedbacks
                        .iter()
                        .take(3)
                        .map(|f| f.chars().take(60).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            };

            db.execute(
                "INSERT OR REPLACE INTO repo_preferences
                 (repo, preferred_types, rejected_types, merge_rate,
                  avg_review_hours, notes, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    repo,
                    serde_json::to_string(&preferred).unwrap_or_default(),
                    serde_json::to_string(&avoid).unwrap_or_default(),
                    (merge_rate * 1000.0).round() / 1000.0,
                    (avg_hours * 10.0).round() / 10.0,
                    notes,
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

            result.repos_profiled += 1;
        }

        // Prune expired working memory
        let now = Utc::now().to_rfc3339();
        let pruned = db
            .execute(
                "DELETE FROM working_memory WHERE expires_at <= ?1",
                params![now],
            )
            .unwrap_or(0);
        result.entries_pruned = pruned;

        // Update dream meta
        db.execute(
            "INSERT OR REPLACE INTO dream_meta (key, value, updated_at)
             VALUES ('last_dream_at', ?1, ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        // Reset session counter
        db.execute(
            "INSERT OR REPLACE INTO dream_meta (key, value, updated_at)
             VALUES ('session_count', '0', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        // Release lock
        db.execute(
            "INSERT OR REPLACE INTO dream_meta (key, value, updated_at)
             VALUES ('dream_lock', '0', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        result.success = true;
        Ok(result)
    }

    /// Get dream stats for display.
    pub fn get_dream_stats(&self) -> Result<HashMap<String, String>> {
        let db = self.lock_db()?;
        let mut stats = HashMap::new();

        let last: String = db
            .query_row(
                "SELECT value FROM dream_meta WHERE key = 'last_dream_at'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "never".into());
        stats.insert("last_dream".into(), last);

        let sessions: String = db
            .query_row(
                "SELECT value FROM dream_meta WHERE key = 'session_count'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "0".into());
        stats.insert("sessions_since_dream".into(), sessions);

        let profiles: i64 = db
            .query_row("SELECT COUNT(*) FROM repo_preferences", [], |r| r.get(0))
            .unwrap_or(0);
        stats.insert("repo_profiles".into(), profiles.to_string());

        Ok(stats)
    }

    /// Get repo leaderboard sorted by merge rate.
    pub fn get_leaderboard(&self, limit: usize) -> Result<Vec<HashMap<String, String>>> {
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT repo, merge_rate, preferred_types, rejected_types
                 FROM repo_preferences
                 ORDER BY merge_rate DESC LIMIT ?1",
            )
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let mut m = HashMap::new();
                m.insert("repo".into(), row.get::<_, String>(0)?);
                m.insert(
                    "merge_rate".into(),
                    format!("{:.0}%", row.get::<_, f64>(1)? * 100.0),
                );
                m.insert("preferred".into(), row.get::<_, String>(2)?);
                m.insert("avoided".into(), row.get::<_, String>(3)?);
                Ok(m)
            })
            .map_err(|e| ContribError::Config(format!("DB error: {}", e)))?;

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Get consolidated repo profile from dream data.
    ///
    /// Returns `None` if no profile exists yet (dream hasn't run for this repo).
    pub fn get_repo_profile(&self, repo: &str) -> Result<Option<RepoPreferences>> {
        let db = self.lock_db()?;
        let result = db.query_row(
            "SELECT preferred_types, rejected_types, merge_rate, avg_review_hours, notes
             FROM repo_preferences WHERE repo = ?1",
            params![repo],
            |row| {
                let preferred_str: String = row.get(0)?;
                let rejected_str: String = row.get(1)?;
                let merge_rate: f64 = row.get(2)?;
                let avg_review_hours: f64 = row.get(3)?;
                let notes: String = row.get(4)?;

                let preferred: Vec<String> =
                    serde_json::from_str(&preferred_str).unwrap_or_default();
                let rejected: Vec<String> = serde_json::from_str(&rejected_str).unwrap_or_default();

                Ok(RepoPreferences {
                    preferred_types: preferred,
                    rejected_types: rejected,
                    merge_rate,
                    avg_review_hours,
                    notes,
                })
            },
        );

        match result {
            Ok(profile) => Ok(Some(profile)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(ContribError::Config(format!("DB error: {}", e))),
        }
    }

    // ── PR Conversation Memory ────────────────────────────────────────────────

    /// Record a single message in a PR conversation thread.
    ///
    /// Duplicate comment_ids are silently ignored (UNIQUE constraint).
    pub fn record_conversation(&self, msg: &ConversationMessage) -> Result<()> {
        let repo = &msg.repo;
        let pr_number = msg.pr_number;
        let role = &msg.role;
        let author = &msg.author;
        let body = &msg.body;
        let comment_id = msg.comment_id;
        let is_inline = msg.is_inline;
        let file_path = msg.file_path.as_deref();
        let db = self.lock_db()?;
        let now = Utc::now().to_rfc3339();
        db.execute(
            "INSERT OR IGNORE INTO pr_conversations
             (repo, pr_number, role, author, body, comment_id, is_inline, file_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                repo,
                pr_number,
                role,
                author,
                body,
                comment_id,
                is_inline as i32,
                file_path,
                now
            ],
        )
        .map_err(|e| ContribError::Config(format!("Failed to record conversation: {}", e)))?;
        Ok(())
    }

    /// Get full conversation context for a PR, formatted for LLM prompts.
    ///
    /// Returns a chronologically-ordered thread like:
    /// ```text
    /// [maintainer @alice] Please use `unwrap_or_default()` instead.
    /// [contribai @contribai-bot] ✅ Fixed — pushed update.
    /// [maintainer @alice] Looks good, thanks!
    /// ```
    pub fn get_conversation_context(&self, repo: &str, pr_number: i64) -> Result<String> {
        let db = self.lock_db()?;
        let mut stmt = db
            .prepare(
                "SELECT role, author, body, file_path, is_inline
                 FROM pr_conversations
                 WHERE repo = ?1 AND pr_number = ?2
                 ORDER BY id ASC",
            )
            .map_err(|e| ContribError::Config(format!("DB prepare: {}", e)))?;

        let rows = stmt
            .query_map(params![repo, pr_number], |row| {
                let role: String = row.get(0)?;
                let author: String = row.get(1)?;
                let body: String = row.get(2)?;
                let file_path: Option<String> = row.get(3)?;
                let is_inline: bool = row.get::<_, i32>(4)? != 0;
                Ok((role, author, body, file_path, is_inline))
            })
            .map_err(|e| ContribError::Config(format!("DB query: {}", e)))?;

        let mut lines = Vec::new();
        for row in rows {
            let (role, author, body, file_path, is_inline) =
                row.map_err(|e| ContribError::Config(format!("DB row: {}", e)))?;

            let location = if is_inline {
                file_path
                    .map(|f| format!(" (on {})", f))
                    .unwrap_or_default()
            } else {
                String::new()
            };

            lines.push(format!("[{} @{}{}] {}", role, author, location, body));
        }

        Ok(lines.join("\n"))
    }

    /// Count conversation messages for a PR.
    pub fn get_conversation_count(&self, repo: &str, pr_number: i64) -> Result<usize> {
        let db = self.lock_db()?;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM pr_conversations WHERE repo = ?1 AND pr_number = ?2",
                params![repo, pr_number],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(count as usize)
    }
}

/// Result of a dream consolidation pass.
#[derive(Debug, Default)]
pub struct DreamResult {
    pub success: bool,
    pub repos_profiled: usize,
    pub entries_pruned: usize,
}

/// Learned repo preferences.
#[derive(Debug, Clone)]
pub struct RepoPreferences {
    pub preferred_types: Vec<String>,
    pub rejected_types: Vec<String>,
    pub merge_rate: f64,
    pub avg_review_hours: f64,
    pub notes: String,
}

/// Helper: convert a PR row to HashMap.
fn pr_row_to_map(row: &rusqlite::Row) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("repo".into(), row.get::<_, String>(0).unwrap_or_default());
    m.insert(
        "pr_number".into(),
        row.get::<_, i64>(1).unwrap_or(0).to_string(),
    );
    m.insert("pr_url".into(), row.get::<_, String>(2).unwrap_or_default());
    m.insert("title".into(), row.get::<_, String>(3).unwrap_or_default());
    m.insert("type".into(), row.get::<_, String>(4).unwrap_or_default());
    m.insert("status".into(), row.get::<_, String>(5).unwrap_or_default());
    m.insert("branch".into(), row.get::<_, String>(6).unwrap_or_default());
    m.insert("fork".into(), row.get::<_, String>(7).unwrap_or_default());
    m.insert(
        "created_at".into(),
        row.get::<_, String>(8).unwrap_or_default(),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::admission::{AdmissionAuditDecision, AdmissionAuditStage};

    fn test_memory() -> Memory {
        Memory::open_in_memory().unwrap()
    }

    #[test]
    fn test_analyzed_repos() {
        let mem = test_memory();
        assert!(!mem.has_analyzed("test/repo").unwrap());

        mem.record_analysis("test/repo", "python", 100, 5).unwrap();
        assert!(mem.has_analyzed("test/repo").unwrap());
    }

    #[test]
    fn test_analyzed_since() {
        let mem = test_memory();
        assert!(!mem.has_analyzed_since("test/repo", 7).unwrap());

        mem.record_analysis("test/repo", "python", 100, 5).unwrap();
        // Just recorded — should be within 7 days
        assert!(mem.has_analyzed_since("test/repo", 7).unwrap());
        // 0-day window should still match (within same day)
        assert!(mem.has_analyzed_since("test/repo", 0).unwrap());
    }

    #[test]
    fn test_pr_recording() {
        let mem = test_memory();
        mem.record_pr(
            "test/repo",
            42,
            "https://github.com/test/repo/pull/42",
            "fix: issue",
            "code_quality",
            "fix/issue",
            "fork/repo",
        )
        .unwrap();

        let prs = mem.get_prs(None, 10).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0]["pr_number"], "42");
    }

    #[test]
    fn test_pr_status_update() {
        let mem = test_memory();
        mem.record_pr("test/repo", 1, "url", "title", "fix", "branch", "fork")
            .unwrap();

        mem.update_pr_status("test/repo", 1, "merged").unwrap();
        let prs = mem.get_prs(Some("merged"), 10).unwrap();
        assert_eq!(prs.len(), 1);
    }

    #[test]
    fn test_today_pr_count() {
        let mem = test_memory();
        mem.record_pr("a/b", 1, "url1", "t1", "fix", "", "")
            .unwrap();
        mem.record_pr("a/b", 2, "url2", "t2", "fix", "", "")
            .unwrap();

        let count = mem.get_today_pr_count().unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_run_log() {
        let mem = test_memory();
        let run_id = mem.start_run().unwrap();
        assert!(run_id > 0);

        mem.finish_run(run_id, 5, 2, 10, 1).unwrap();
        let stats = mem.get_stats().unwrap();
        assert_eq!(stats["total_runs"], 1);
    }

    #[test]
    fn test_outcome_learning() {
        let mem = test_memory();

        mem.record_outcome("test/repo", 1, "url1", "security_fix", "merged", "", 24.0)
            .unwrap();
        mem.record_outcome(
            "test/repo",
            2,
            "url2",
            "code_quality",
            "closed",
            "not needed",
            48.0,
        )
        .unwrap();
        mem.record_outcome("test/repo", 3, "url3", "security_fix", "merged", "", 12.0)
            .unwrap();

        let prefs = mem.get_repo_preferences("test/repo").unwrap().unwrap();
        assert!(prefs.preferred_types.contains(&"security_fix".to_string()));
        assert!(prefs.rejected_types.contains(&"code_quality".to_string()));
        assert!((prefs.merge_rate - 0.667).abs() < 0.01);
        assert!(prefs.avg_review_hours > 0.0);
    }

    #[test]
    fn test_working_memory() {
        let mem = test_memory();

        mem.store_context("test/repo", "style", "4 spaces indent", "python", 24.0)
            .unwrap();
        let val = mem.get_context("test/repo", "style").unwrap();
        assert_eq!(val, Some("4 spaces indent".to_string()));

        let missing = mem.get_context("test/repo", "nonexistent").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_similar_context() {
        let mem = test_memory();
        mem.store_context("repo/a", "style", "PEP 8", "python", 24.0)
            .unwrap();
        mem.store_context("repo/b", "style", "Black format", "python", 24.0)
            .unwrap();
        mem.store_context("repo/c", "style", "gofmt", "go", 24.0)
            .unwrap();

        let similar = mem.get_similar_context("python", "style", 10).unwrap();
        assert_eq!(similar.len(), 2);
    }

    #[test]
    fn test_stats() {
        let mem = test_memory();
        mem.record_analysis("a/b", "python", 100, 5).unwrap();
        mem.record_pr("a/b", 1, "url", "t", "fix", "", "").unwrap();
        mem.update_pr_status("a/b", 1, "merged").unwrap();

        let stats = mem.get_stats().unwrap();
        assert_eq!(stats["total_repos_analyzed"], 1);
        assert_eq!(stats["total_prs_submitted"], 1);
        assert_eq!(stats["prs_merged"], 1);
    }

    fn audit_record(repository: &str, decision: AdmissionAuditDecision) -> AdmissionAuditRecord {
        AdmissionAuditRecord {
            schema_version: 1,
            receipt: String::new(),
            previous_receipt: None,
            repository: repository.to_string(),
            run_id: None,
            contribution_fingerprint: "f".repeat(64),
            stage: AdmissionAuditStage::Admission,
            decision,
            reason: "test decision".to_string(),
            base_sha: Some("0".repeat(40)),
            permit_id: Some("1".repeat(24)),
            issue: None,
            file_count: 1,
            changed_lines: 2,
            paths: vec!["src/lib.rs".to_string()],
            violations: Vec::new(),
            checks: Vec::new(),
            recorded_at: Utc::now(),
        }
    }

    #[test]
    fn admission_audit_is_append_only_filterable_and_linked() {
        let mem = test_memory();
        let first = mem
            .record_admission_audit(audit_record("owner/one", AdmissionAuditDecision::Blocked))
            .unwrap();
        let second = mem
            .record_admission_audit(audit_record("owner/two", AdmissionAuditDecision::Approved))
            .unwrap();

        assert!(first.verify_receipt());
        assert_eq!(
            second.previous_receipt.as_deref(),
            Some(first.receipt.as_str())
        );
        assert!(second.verify_receipt());
        assert_eq!(
            mem.get_admission_audits(Some("owner/two"), Some("approved"), 10)
                .unwrap(),
            vec![second]
        );
        assert!(mem.verify_admission_audit_chain().unwrap().valid);
    }

    #[test]
    fn admission_audit_chain_detects_payload_tampering() {
        let mem = test_memory();
        mem.record_admission_audit(audit_record("owner/repo", AdmissionAuditDecision::Approved))
            .unwrap();
        {
            let db = mem.lock_db().unwrap();
            let payload: String = db
                .query_row("SELECT payload FROM admission_audit", [], |row| row.get(0))
                .unwrap();
            let tampered = payload.replace("test decision", "changed decision");
            db.execute("UPDATE admission_audit SET payload = ?1", params![tampered])
                .unwrap();
        }
        let verification = mem.verify_admission_audit_chain().unwrap();
        assert!(!verification.valid);
        assert_eq!(verification.records_checked, 1);
    }

    #[test]
    fn admission_audit_chain_detects_index_column_tampering() {
        let mem = test_memory();
        mem.record_admission_audit(audit_record("owner/repo", AdmissionAuditDecision::Approved))
            .unwrap();
        {
            let db = mem.lock_db().unwrap();
            db.execute("UPDATE admission_audit SET decision = 'blocked'", [])
                .unwrap();
        }
        let verification = mem.verify_admission_audit_chain().unwrap();
        assert!(!verification.valid);
        assert_eq!(verification.records_checked, 1);
    }

    fn audit_payloads(mem: &Memory) -> Vec<String> {
        let db = mem.lock_db().unwrap();
        let mut query = db
            .prepare("SELECT payload FROM admission_audit ORDER BY id")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn admission_audit_append_rejects_corruption_anywhere_in_history() {
        for mutation in [
            "UPDATE admission_audit SET payload = replace(payload, 'test decision', 'modified') WHERE id = 1",
            "UPDATE admission_audit SET decision = 'blocked' WHERE id = 1",
            "DELETE FROM admission_audit WHERE id = 2",
        ] {
            let mem = test_memory();
            for _ in 0..3 {
                mem.record_admission_audit(audit_record(
                    "owner/repo",
                    AdmissionAuditDecision::Approved,
                ))
                .unwrap();
            }
            mem.lock_db().unwrap().execute(mutation, []).unwrap();
            let before = audit_payloads(&mem);
            let error = mem
                .record_admission_audit(audit_record("owner/repo", AdmissionAuditDecision::Approved))
                .unwrap_err();
            assert!(error.to_string().contains("integrity check failed"));
            assert_eq!(audit_payloads(&mem), before, "must preserve damaged history");
            assert!(!mem.verify_admission_audit_chain().unwrap().valid);
        }
    }

    #[test]
    fn admission_audit_invalid_payload_errors_do_not_expose_stored_values() {
        let mem = test_memory();
        mem.record_admission_audit(audit_record("owner/repo", AdmissionAuditDecision::Approved))
            .unwrap();
        let sensitive_value = "private-repository-content-must-not-be-logged";
        let mut payload: serde_json::Value =
            serde_json::from_str(&audit_payloads(&mem)[0]).unwrap();
        payload["decision"] = sensitive_value.into();
        mem.lock_db()
            .unwrap()
            .execute(
                "UPDATE admission_audit SET payload = ?1",
                params![payload.to_string()],
            )
            .unwrap();
        let before = audit_payloads(&mem);
        for result in [
            mem.verify_admission_audit_chain().map(|_| ()),
            mem.get_admission_audits(None, None, 10).map(|_| ()),
            mem.record_admission_audit(audit_record(
                "owner/repo",
                AdmissionAuditDecision::Approved,
            ))
            .map(|_| ()),
        ] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("payload is invalid"));
            assert!(!error.contains(sensitive_value));
        }
        assert_eq!(audit_payloads(&mem), before);
    }

    #[test]
    fn admission_audit_rejects_unsupported_new_record_schema() {
        let mem = test_memory();
        let mut record = audit_record("owner/repo", AdmissionAuditDecision::Approved);
        record.schema_version = 99;
        assert!(mem.record_admission_audit(record).is_err());
        assert!(audit_payloads(&mem).is_empty());
        assert!(mem.verify_admission_audit_chain().unwrap().valid);
    }

    #[test]
    fn admission_audit_concurrent_connections_preserve_one_chain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.db");
        let connections = [Memory::open(&path).unwrap(), Memory::open(&path).unwrap()];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writers = connections.map(|mem| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..4 {
                    mem.record_admission_audit(audit_record(
                        "owner/repo",
                        AdmissionAuditDecision::Approved,
                    ))
                    .unwrap();
                }
            })
        });
        for writer in writers {
            writer.join().unwrap();
        }
        let mem = Memory::open(&path).unwrap();
        let verified = mem.verify_admission_audit_chain().unwrap();
        assert!(verified.valid);
        assert_eq!(verified.records_checked, 8);
    }

    // ── Dream consolidation tests ─────────────────────────────────────────

    #[test]
    fn test_session_counter() {
        let mem = test_memory();
        assert_eq!(mem.increment_session_count().unwrap(), 1);
        assert_eq!(mem.increment_session_count().unwrap(), 2);
        assert_eq!(mem.increment_session_count().unwrap(), 3);
    }

    #[test]
    fn test_should_dream_gates() {
        let mem = test_memory();

        // No sessions yet → false
        assert!(!mem.should_dream().unwrap());

        // Add 5 sessions → should pass (no prior dream = time gate passes)
        for _ in 0..5 {
            mem.increment_session_count().unwrap();
        }
        assert!(mem.should_dream().unwrap());
    }

    #[test]
    fn test_dream_consolidation() {
        let mem = test_memory();

        // Add outcomes
        mem.record_outcome("repo/a", 1, "url1", "security_fix", "merged", "", 24.0)
            .unwrap();
        mem.record_outcome("repo/a", 2, "url2", "docs", "merged", "good docs", 12.0)
            .unwrap();
        mem.record_outcome("repo/a", 3, "url3", "refactor", "closed", "not needed", 0.0)
            .unwrap();

        mem.record_outcome("repo/b", 10, "url10", "docs", "merged", "", 6.0)
            .unwrap();

        // Fill sessions
        for _ in 0..5 {
            mem.increment_session_count().unwrap();
        }

        // Run dream
        let result = mem.run_dream().unwrap();
        assert!(result.success);
        assert_eq!(result.repos_profiled, 2);

        // Verify profiles
        let prefs_a = mem.get_repo_preferences("repo/a").unwrap().unwrap();
        assert!(prefs_a
            .preferred_types
            .contains(&"security_fix".to_string()));
        assert!(prefs_a.preferred_types.contains(&"docs".to_string()));
        assert!(prefs_a.merge_rate > 0.6);

        let prefs_b = mem.get_repo_preferences("repo/b").unwrap().unwrap();
        assert_eq!(prefs_b.merge_rate, 1.0);

        // After dream, session counter should be reset
        assert!(!mem.should_dream().unwrap());
    }

    #[test]
    fn test_dream_stats() {
        let mem = test_memory();
        let stats = mem.get_dream_stats().unwrap();
        assert_eq!(stats["last_dream"], "never");
        assert_eq!(stats["sessions_since_dream"], "0");
    }

    #[test]
    fn test_leaderboard() {
        let mem = test_memory();

        mem.record_outcome("repo/a", 1, "u", "fix", "merged", "", 10.0)
            .unwrap();
        mem.record_outcome("repo/b", 1, "u", "fix", "closed", "", 10.0)
            .unwrap();
        mem.record_outcome("repo/b", 2, "u", "fix", "merged", "", 10.0)
            .unwrap();

        let board = mem.get_leaderboard(10).unwrap();
        assert!(!board.is_empty());
        // repo/a has 100% merge rate, should be first
        assert_eq!(board[0]["repo"], "repo/a");
    }

    // ── Contribution run persistence ─────────────────────────────────────

    fn test_run() -> ContributionRun {
        ContributionRun::new(
            "owner/repo",
            Some(7),
            "0123456789abcdef0123456789abcdef01234567",
            Utc::now() + Duration::hours(24),
        )
    }

    #[test]
    fn run_insert_load_roundtrip() {
        let mem = test_memory();
        let run = test_run();
        mem.insert_run(&run).unwrap();
        let loaded = mem.get_run(&run.run_id).unwrap().expect("run exists");
        assert_eq!(loaded.run_id, run.run_id);
        assert_eq!(loaded.repository, "owner/repo");
        assert_eq!(loaded.issue, Some(7));
        assert_eq!(loaded.state, RunState::Discovered);
        // Duplicate run_id is rejected, never silently merged.
        assert!(mem.insert_run(&run).is_err());
    }

    #[test]
    fn transition_run_persists_state_and_event_atomically() {
        let mem = test_memory();
        let mut run = test_run();
        run.permit_id = Some("1".repeat(24));
        mem.insert_run(&run).unwrap();

        let (run, event) = mem
            .transition_run(
                &run.run_id,
                RunState::Authorized,
                "authorize",
                "permit issued",
            )
            .unwrap();
        assert_eq!(run.state, RunState::Authorized);
        assert_eq!(event.state_from, RunState::Discovered);

        let loaded = mem.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(loaded.state, RunState::Authorized);

        let events = mem.get_run_events(&run.run_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "authorize");
        assert_eq!(events[0].detail, "permit issued");
    }

    #[test]
    fn invalid_transition_fails_closed_and_leaves_no_event() {
        let mem = test_memory();
        let run = test_run();
        mem.insert_run(&run).unwrap();
        // Discovered → Submitted is not a valid transition.
        assert!(mem
            .transition_run(&run.run_id, RunState::Submitted, "skip", "")
            .is_err());
        assert_eq!(
            mem.get_run(&run.run_id).unwrap().unwrap().state,
            RunState::Discovered
        );
        assert!(mem.get_run_events(&run.run_id).unwrap().is_empty());
    }

    #[test]
    fn stale_state_cannot_transition_twice() {
        let mem = test_memory();
        let mut run = test_run();
        run.permit_id = Some("1".repeat(24));
        mem.insert_run(&run).unwrap();
        mem.transition_run(&run.run_id, RunState::Authorized, "a", "")
            .unwrap();
        // Second attempt: Authorized → Authorized is invalid.
        assert!(mem
            .transition_run(&run.run_id, RunState::Authorized, "a2", "")
            .is_err());
        let events = mem.get_run_events(&run.run_id).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn interrupted_runs_lists_only_nonterminal() {
        let mem = test_memory();
        let mut active = test_run();
        active.permit_id = Some("1".repeat(24));
        mem.insert_run(&active).unwrap();
        mem.transition_run(&active.run_id, RunState::Authorized, "a", "")
            .unwrap();

        // A second run needs a distinct id — different issue.
        let done = ContributionRun::new(
            "owner/repo",
            Some(8),
            "0123456789abcdef0123456789abcdef01234567",
            Utc::now() + Duration::hours(24),
        );
        mem.insert_run(&done).unwrap();
        mem.transition_run(&done.run_id, RunState::Cancelled, "cancel", "user")
            .unwrap();

        let interrupted = mem.list_interrupted_runs().unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].run_id, active.run_id);
    }

    #[test]
    fn update_run_persists_fingerprints() {
        let mem = test_memory();
        let mut run = test_run();
        mem.insert_run(&run).unwrap();
        run.candidate_fingerprint = Some("c".repeat(64));
        run.task_fingerprint = Some("t".repeat(64));
        run.reproduction = Some("reproduced".into());
        mem.update_run(&run).unwrap();
        let loaded = mem.get_run(&run.run_id).unwrap().unwrap();
        assert_eq!(
            loaded.candidate_fingerprint.as_deref(),
            Some("c".repeat(64).as_str())
        );
        assert_eq!(loaded.reproduction.as_deref(), Some("reproduced"));
    }
}
