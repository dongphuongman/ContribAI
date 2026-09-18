//! Isolated run workspace.
//!
//! Every mutation-capable Contribution Run operates in an isolated workspace
//! bound to the attested base SHA. Three materialization strategies exist:
//!
//! - **worktree**: `git worktree add --detach` from a local clone (preferred).
//! - **clone**: init + fetch the pinned SHA into a fresh directory.
//! - **snapshot**: plain files fetched via the API (no git metadata).
//!
//! Isolation guarantees:
//! - every path resolve rejects `..`, separators, absolute paths, and symlink
//!   escapes (writes can never traverse a symlink);
//! - workspaces live under `<runs_root>/<run_id>` — runs cannot collide;
//! - `has_unexpected_changes` detects foreign edits after a checkpoint;
//! - cleanup never deletes a dirty tree unless told to preserve state.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::core::admission::repository_path_error;
use crate::core::error::{ContribError, Result};
use crate::core::review_surface::{line_diff_counts, ChangedFile};

/// How the workspace was materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStrategy {
    Worktree,
    Clone,
    /// No git metadata — an API-fetched file snapshot.
    Snapshot,
}

/// Persisted workspace metadata (sibling `<run_id>.workspace.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub run_id: String,
    pub base_sha: String,
    pub strategy: WorkspaceStrategy,
    /// For worktree: the source repository the worktree belongs to.
    pub source_repo: Option<PathBuf>,
    /// Fingerprint of the tree at the last checkpoint, if taken.
    pub checkpoint_fingerprint: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Recorded change for snapshot workspaces (no git to diff against).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotChange {
    path: String,
    lines_added: usize,
    lines_deleted: usize,
    is_binary: bool,
    /// SHA-256 of the bytes we wrote — lets us distinguish our edits from
    /// foreign modifications.
    content_sha: String,
}

/// An isolated workspace bound to one run.
pub struct RunWorkspace {
    root: PathBuf,
    canonical_root: PathBuf,
    meta_path: PathBuf,
    changes_path: PathBuf,
    meta: WorkspaceMeta,
}

impl RunWorkspace {
    // ── Constructors ────────────────────────────────────────────────────

    /// Create a detached git worktree at `base_sha` inside `<runs_root>/<run_id>`.
    pub async fn from_worktree(
        source_repo: &Path,
        runs_root: &Path,
        run_id: &str,
        base_sha: &str,
    ) -> Result<Self> {
        validate_run_id(run_id)?;
        let root = runs_root.join(run_id);
        if root.exists() {
            return Err(ContribError::Config(format!(
                "workspace {} already exists",
                root.display()
            )));
        }
        std::fs::create_dir_all(runs_root)
            .map_err(|e| ContribError::Config(format!("runs root: {e}")))?;
        let output = Command::new("git")
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "worktree",
                "add",
                "--detach",
                &root.to_string_lossy(),
                base_sha,
            ])
            .output()
            .await
            .map_err(|e| ContribError::Config(format!("git worktree spawn: {e}")))?;
        if !output.status.success() {
            return Err(ContribError::Config(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(300)
                    .collect::<String>()
            )));
        }
        Self::finish_open(
            root,
            runs_root,
            run_id,
            base_sha,
            WorkspaceMeta {
                run_id: run_id.to_string(),
                base_sha: base_sha.to_string(),
                strategy: WorkspaceStrategy::Worktree,
                source_repo: Some(source_repo.to_path_buf()),
                checkpoint_fingerprint: None,
                created_at: Utc::now(),
            },
        )
    }

    /// Clone a remote repository at the pinned SHA. Uses a depth-1 SHA fetch
    /// when the host supports it; falls back to a full `--no-checkout` clone.
    pub async fn from_clone(
        repo_url: &str,
        runs_root: &Path,
        run_id: &str,
        base_sha: &str,
    ) -> Result<Self> {
        validate_run_id(run_id)?;
        let root = runs_root.join(run_id);
        if root.exists() {
            return Err(ContribError::Config(format!(
                "workspace {} already exists",
                root.display()
            )));
        }
        std::fs::create_dir_all(runs_root)
            .map_err(|e| ContribError::Config(format!("runs root: {e}")))?;

        // Attempt the cheap path: init + depth-1 fetch of the exact SHA.
        let shallow = Command::new("git")
            .args(["init", &root.to_string_lossy()])
            .output()
            .await;
        let mut cloned = false;
        if shallow.map(|o| o.status.success()).unwrap_or(false) {
            let remote = Command::new("git")
                .args([
                    "-C",
                    &root.to_string_lossy(),
                    "remote",
                    "add",
                    "origin",
                    repo_url,
                ])
                .output()
                .await;
            let fetch = Command::new("git")
                .args([
                    "-C",
                    &root.to_string_lossy(),
                    "fetch",
                    "--depth",
                    "1",
                    "origin",
                    base_sha,
                ])
                .output()
                .await;
            let checkout = Command::new("git")
                .args(["-C", &root.to_string_lossy(), "checkout", "FETCH_HEAD"])
                .output()
                .await;
            cloned = remote.map(|o| o.status.success()).unwrap_or(false)
                && fetch.map(|o| o.status.success()).unwrap_or(false)
                && checkout.map(|o| o.status.success()).unwrap_or(false);
        }
        if !cloned {
            // Full no-checkout clone, then check out the pinned SHA.
            let _ = std::fs::remove_dir_all(&root);
            let clone = Command::new("git")
                .args(["clone", "--no-checkout", repo_url, &root.to_string_lossy()])
                .output()
                .await
                .map_err(|e| ContribError::Config(format!("git clone spawn: {e}")))?;
            if !clone.status.success() {
                return Err(ContribError::Config(format!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&clone.stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                )));
            }
            let checkout = Command::new("git")
                .args(["-C", &root.to_string_lossy(), "checkout", base_sha])
                .output()
                .await
                .map_err(|e| ContribError::Config(format!("git checkout spawn: {e}")))?;
            if !checkout.status.success() {
                return Err(ContribError::Config(format!(
                    "pinned checkout of {base_sha} failed"
                )));
            }
        }
        Self::finish_open(
            root,
            runs_root,
            run_id,
            base_sha,
            WorkspaceMeta {
                run_id: run_id.to_string(),
                base_sha: base_sha.to_string(),
                strategy: WorkspaceStrategy::Clone,
                source_repo: None,
                checkpoint_fingerprint: None,
                created_at: Utc::now(),
            },
        )
    }

    /// Materialize plain files fetched via the API — no git metadata.
    ///
    /// `files` are `(repo_relative_path, bytes)` pairs; each path is validated
    /// with the same rules the admission layer enforces.
    pub async fn materialize(
        runs_root: &Path,
        run_id: &str,
        base_sha: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<Self> {
        validate_run_id(run_id)?;
        let root = runs_root.join(run_id);
        if root.exists() {
            return Err(ContribError::Config(format!(
                "workspace {} already exists",
                root.display()
            )));
        }
        let mut workspace = Self::finish_open(
            root.clone(),
            runs_root,
            run_id,
            base_sha,
            WorkspaceMeta {
                run_id: run_id.to_string(),
                base_sha: base_sha.to_string(),
                strategy: WorkspaceStrategy::Snapshot,
                source_repo: None,
                checkpoint_fingerprint: None,
                created_at: Utc::now(),
            },
        )?;
        for (path, bytes) in files {
            workspace.write_file(path, bytes)?;
        }
        // The materialized files are the baseline, not changes.
        workspace.clear_changes()?;
        workspace.checkpoint().await?;
        Ok(workspace)
    }

    /// Re-open an existing workspace after a crash/restart.
    pub fn open_existing(runs_root: &Path, run_id: &str) -> Result<Self> {
        let meta_path = runs_root.join(format!("{run_id}.workspace.json"));
        let raw = std::fs::read_to_string(&meta_path)
            .map_err(|e| ContribError::Config(format!("workspace meta read: {e}")))?;
        let meta: WorkspaceMeta = serde_json::from_str(&raw)
            .map_err(|e| ContribError::Config(format!("workspace meta parse: {e}")))?;
        if meta.run_id != run_id {
            return Err(ContribError::Config(
                "workspace meta run_id mismatch".into(),
            ));
        }
        let root = runs_root.join(run_id);
        let canonical_root = canonicalize_root(&root)?;
        Ok(Self {
            root,
            canonical_root,
            meta_path,
            changes_path: runs_root.join(format!("{run_id}.changes.json")),
            meta,
        })
    }

    fn finish_open(
        root: PathBuf,
        runs_root: &Path,
        run_id: &str,
        base_sha: &str,
        meta: WorkspaceMeta,
    ) -> Result<Self> {
        if meta.base_sha != base_sha {
            return Err(ContribError::Config("workspace base SHA mismatch".into()));
        }
        let canonical_root = canonicalize_root(&root)?;
        let workspace = Self {
            root,
            canonical_root,
            meta_path: runs_root.join(format!("{run_id}.workspace.json")),
            changes_path: runs_root.join(format!("{run_id}.changes.json")),
            meta,
        };
        workspace.save_meta()?;
        Ok(workspace)
    }

    // ── Inspection ──────────────────────────────────────────────────────

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base_sha(&self) -> &str {
        &self.meta.base_sha
    }

    pub fn strategy(&self) -> WorkspaceStrategy {
        self.meta.strategy
    }

    /// Resolve a repo-relative path inside the workspace.
    ///
    /// Rejects anything `repository_path_error` rejects, plus any path whose
    /// existing ancestor is a symlink — writes can never traverse a link.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf> {
        if let Some(reason) = repository_path_error(rel) {
            return Err(ContribError::Config(format!(
                "unsafe path {rel:?}: {reason}"
            )));
        }
        let joined = self.root.join(rel);
        // Walk existing ancestors: any symlink in the chain is an escape.
        let mut cursor = self.root.clone();
        for component in Path::new(rel).components() {
            let Component::Normal(part) = component else {
                return Err(ContribError::Config(format!(
                    "unsafe path {rel:?}: non-normal component"
                )));
            };
            cursor.push(part);
            if let Ok(meta) = std::fs::symlink_metadata(&cursor) {
                if meta.file_type().is_symlink() {
                    return Err(ContribError::Config(format!(
                        "unsafe path {rel:?}: symlink in path chain"
                    )));
                }
            }
        }
        // Existing paths must canonicalize inside the workspace root —
        // catches junctions/reparse points the symlink walk cannot see.
        if joined.exists() {
            let canonical = std::fs::canonicalize(&joined)
                .map_err(|e| ContribError::Config(format!("resolve {rel:?}: {e}")))?;
            if !canonical.starts_with(&self.canonical_root) {
                return Err(ContribError::Config(format!(
                    "unsafe path {rel:?}: escapes workspace root"
                )));
            }
        }
        Ok(joined)
    }

    /// Read a workspace file; symlinked entries are rejected outright.
    pub fn read_file(&self, rel: &str) -> Result<Vec<u8>> {
        let path = self.resolve(rel)?;
        std::fs::read(&path).map_err(|e| ContribError::Config(format!("read {rel}: {e}")))
    }

    /// Write a workspace file, recording the change for snapshot mode.
    pub fn write_file(&self, rel: &str, content: &[u8]) -> Result<()> {
        let path = self.resolve(rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ContribError::Config(format!("mkdir for {rel}: {e}")))?;
        }
        let before = std::fs::read(&path).unwrap_or_default();
        std::fs::write(&path, content)
            .map_err(|e| ContribError::Config(format!("write {rel}: {e}")))?;
        self.record_change(rel, &before, content)
    }

    /// Delete a workspace file (subject to the same path policy).
    pub fn delete_file(&self, rel: &str) -> Result<()> {
        let path = self.resolve(rel)?;
        let before = std::fs::read(&path).unwrap_or_default();
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| ContribError::Config(format!("delete {rel}: {e}")))?;
            self.record_change(rel, &before, b"")?;
        }
        Ok(())
    }

    /// Changed files vs the base: `git diff --numstat` for git workspaces,
    /// the recorded change list for snapshots.
    pub async fn changed_files(&self) -> Result<Vec<ChangedFile>> {
        match self.meta.strategy {
            WorkspaceStrategy::Snapshot => Ok(self
                .snapshot_changes()?
                .into_iter()
                .map(|c| ChangedFile {
                    path: c.path,
                    lines_added: c.lines_added,
                    lines_deleted: c.lines_deleted,
                    is_binary: c.is_binary,
                })
                .collect()),
            _ => self.git_changed_files().await,
        }
    }

    /// Fingerprint of the current tree state — cheap for git (status
    /// porcelain + HEAD), content-based for snapshots.
    pub async fn tree_fingerprint(&self) -> Result<String> {
        match self.meta.strategy {
            WorkspaceStrategy::Snapshot => self.snapshot_tree_fingerprint(),
            _ => {
                let status = self.git(&["status", "--porcelain"]).await?;
                let head = self.git(&["rev-parse", "HEAD"]).await?;
                let mut digest = Sha256::new();
                digest.update(head.trim().as_bytes());
                digest.update(status.as_bytes());
                Ok(hex::encode(digest.finalize()))
            }
        }
    }

    /// Record the current tree state as the expected checkpoint.
    pub async fn checkpoint(&mut self) -> Result<()> {
        self.meta.checkpoint_fingerprint = Some(self.tree_fingerprint().await?);
        self.save_meta()
    }

    /// Whether the tree has drifted from the last checkpoint — i.e., changes
    /// the run did not record (foreign edits, crashed-write leftovers).
    /// With no checkpoint, any change vs base counts as unexpected.
    pub async fn has_unexpected_changes(&self) -> Result<bool> {
        let current = self.tree_fingerprint().await?;
        Ok(match &self.meta.checkpoint_fingerprint {
            Some(expected) => current != *expected,
            None => {
                // No checkpoint: for git, any diff vs base is unexpected;
                // for snapshot, any recorded change is expected.
                match self.meta.strategy {
                    WorkspaceStrategy::Snapshot => false,
                    _ => {
                        let status = self.git(&["status", "--porcelain"]).await?;
                        !status.trim().is_empty()
                    }
                }
            }
        })
    }

    /// Remove the workspace. A dirty tree is preserved unless `force` —
    /// destructive cleanup is never silent.
    pub async fn cleanup(self, force: bool) -> Result<()> {
        if !force && self.has_unexpected_changes().await? {
            return Err(ContribError::Config(format!(
                "workspace {} has unexpected changes; refusing cleanup",
                self.root.display()
            )));
        }
        if self.meta.strategy == WorkspaceStrategy::Worktree {
            if let Some(source) = &self.meta.source_repo {
                let output = Command::new("git")
                    .args([
                        "-C",
                        &source.to_string_lossy(),
                        "worktree",
                        "remove",
                        "--force",
                        &self.root.to_string_lossy(),
                    ])
                    .output()
                    .await;
                if output.map(|o| o.status.success()).unwrap_or(false) {
                    let _ = std::fs::remove_file(&self.meta_path);
                    let _ = std::fs::remove_file(&self.changes_path);
                    return Ok(());
                }
            }
        }
        std::fs::remove_dir_all(&self.root)
            .map_err(|e| ContribError::Config(format!("workspace cleanup: {e}")))?;
        let _ = std::fs::remove_file(&self.meta_path);
        let _ = std::fs::remove_file(&self.changes_path);
        Ok(())
    }

    // ── Internals ───────────────────────────────────────────────────────

    async fn git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ContribError::Config(format!("git spawn: {e}")))?;
        if !output.status.success() {
            return Err(ContribError::Config(format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn git_changed_files(&self) -> Result<Vec<ChangedFile>> {
        // Tracked modifications/additions vs base.
        let numstat = self
            .git(&["diff", "--numstat", &self.meta.base_sha, "--"])
            .await?;
        let mut out = Vec::new();
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            let (added, deleted) = match (parts.next(), parts.next()) {
                (Some("-"), Some("-")) => (0, 0), // binary
                (Some(a), Some(d)) => (a.parse().unwrap_or(0), d.parse().unwrap_or(0)),
                _ => continue,
            };
            if let Some(path) = parts.next() {
                out.push(ChangedFile {
                    path: path.replace('\\', "/"),
                    lines_added: added,
                    lines_deleted: deleted,
                    is_binary: line.starts_with("-\t-\t"),
                });
            }
        }
        // Untracked files aren't in `diff` — count them via status.
        let status = self.git(&["status", "--porcelain"]).await?;
        for line in status.lines() {
            if let Some(path) = line.strip_prefix("?? ") {
                let path = path.trim().replace('\\', "/");
                if out.iter().any(|c| c.path == path) {
                    continue;
                }
                let bytes = self.read_file(&path).unwrap_or_default();
                out.push(ChangedFile {
                    path,
                    lines_added: byte_line_count(&bytes),
                    lines_deleted: 0,
                    is_binary: bytes.contains(&0),
                });
            }
        }
        Ok(out)
    }

    fn record_change(&self, rel: &str, before: &[u8], after: &[u8]) -> Result<()> {
        if self.meta.strategy != WorkspaceStrategy::Snapshot {
            return Ok(());
        }
        let mut changes = self.snapshot_changes()?;
        changes.retain(|c| c.path != rel);
        let (added, deleted) = if before.is_empty() && !after.is_empty() {
            (byte_line_count(after), 0)
        } else if after.is_empty() && !before.is_empty() {
            (0, byte_line_count(before))
        } else {
            line_diff_counts(
                std::str::from_utf8(before).unwrap_or(""),
                std::str::from_utf8(after).unwrap_or(""),
            )
        };
        changes.push(SnapshotChange {
            path: rel.to_string(),
            lines_added: added,
            lines_deleted: deleted,
            is_binary: after.contains(&0),
            content_sha: hex::encode(Sha256::digest(after)),
        });
        let raw = serde_json::to_string(&changes)
            .map_err(|e| ContribError::Config(format!("changes encode: {e}")))?;
        std::fs::write(&self.changes_path, raw)
            .map_err(|e| ContribError::Config(format!("changes write: {e}")))?;
        Ok(())
    }

    fn clear_changes(&self) -> Result<()> {
        if self.changes_path.exists() {
            std::fs::remove_file(&self.changes_path)
                .map_err(|e| ContribError::Config(format!("changes clear: {e}")))?;
        }
        Ok(())
    }

    fn snapshot_changes(&self) -> Result<Vec<SnapshotChange>> {
        if !self.changes_path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.changes_path)
            .map_err(|e| ContribError::Config(format!("changes read: {e}")))?;
        serde_json::from_str(&raw).map_err(|e| ContribError::Config(format!("changes parse: {e}")))
    }

    /// Snapshot fingerprint covers the recorded changes AND whether each
    /// recorded file still matches the bytes we wrote. A foreign edit to a
    /// recorded file changes its content hash → detected.
    fn snapshot_tree_fingerprint(&self) -> Result<String> {
        let mut digest = Sha256::new();
        let changes: BTreeMap<String, SnapshotChange> = self
            .snapshot_changes()?
            .into_iter()
            .map(|c| (c.path.clone(), c))
            .collect();
        for (path, change) in &changes {
            digest.update(path.as_bytes());
            digest.update(change.content_sha.as_bytes());
            let current = self.read_file(path).unwrap_or_default();
            digest.update(Sha256::digest(&current));
        }
        Ok(hex::encode(digest.finalize()))
    }

    fn save_meta(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.meta)
            .map_err(|e| ContribError::Config(format!("workspace meta: {e}")))?;
        std::fs::write(&self.meta_path, raw)
            .map_err(|e| ContribError::Config(format!("workspace meta write: {e}")))?;
        Ok(())
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.len() != 28
        || !run_id.starts_with("run_")
        || !run_id[4..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ContribError::Config(format!("malformed run id {run_id:?}")));
    }
    Ok(())
}

fn canonicalize_root(root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)
        .map_err(|e| ContribError::Config(format!("workspace root: {e}")))?;
    std::fs::canonicalize(root)
        .map_err(|e| ContribError::Config(format!("workspace canonicalize: {e}")))
}

fn byte_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut count = bytes.iter().filter(|b| **b == b'\n').count();
    if !bytes.ends_with(b"\n") {
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const ID: &str = "run_0123456789abcdef01234567";

    #[test]
    fn snapshot_workspace_records_and_reports_changes() {
        let root = runs_root();
        let ws = tokio_test::block_on(RunWorkspace::materialize(
            root.path(),
            ID,
            SHA,
            &[("src/lib.rs".into(), b"fn a() {}\n".to_vec())],
        ))
        .unwrap();
        // Baseline has no changes.
        assert!(tokio_test::block_on(ws.changed_files()).unwrap().is_empty());
        ws.write_file("src/lib.rs", b"fn a() {}\nfn b() {}\n")
            .unwrap();
        ws.write_file("src/new.rs", b"fn n() {}\n").unwrap();
        let changes = tokio_test::block_on(ws.changed_files()).unwrap();
        assert_eq!(changes.len(), 2);
        let lib = changes.iter().find(|c| c.path == "src/lib.rs").unwrap();
        assert_eq!((lib.lines_added, lib.lines_deleted), (1, 0));
    }

    #[test]
    fn path_traversal_and_symlink_escape_are_denied() {
        let root = runs_root();
        let ws =
            tokio_test::block_on(RunWorkspace::materialize(root.path(), ID, SHA, &[])).unwrap();
        for bad in [
            "../escape.rs",
            "src/../../escape.rs",
            "src\\evil.rs",
            "/abs.rs",
            "C:/evil.rs",
            "a//b.rs",
            "./x.rs",
        ] {
            assert!(ws.resolve(bad).is_err(), "path {bad:?} must be denied");
        }
        // Symlink escape: link inside tree pointing outside.
        let outside = root.path().join("outside_secret.txt");
        std::fs::write(&outside, b"secret").unwrap();
        let link = ws.root().join("link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(ws.resolve("link.txt").is_err());
            assert!(ws.write_file("link.txt", b"x").is_err());
        }
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_file(&outside, &link).is_ok() {
                assert!(ws.resolve("link.txt").is_err());
                assert!(ws.write_file("link.txt", b"x").is_err());
            }
            // No symlink privilege on this host — skip the assertion.
        }
    }

    #[test]
    fn workspace_dirs_cannot_collide() {
        let root = runs_root();
        tokio_test::block_on(RunWorkspace::materialize(root.path(), ID, SHA, &[])).unwrap();
        assert!(
            tokio_test::block_on(RunWorkspace::materialize(root.path(), ID, SHA, &[])).is_err()
        );
    }

    #[test]
    fn malformed_run_ids_are_rejected() {
        let root = runs_root();
        for bad in ["run_short", "../escape", "run_zzzzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(
                tokio_test::block_on(RunWorkspace::materialize(root.path(), bad, SHA, &[]))
                    .is_err()
            );
        }
    }

    #[test]
    fn reopening_preserves_state_and_detects_foreign_edits() {
        let root = runs_root();
        let mut ws = tokio_test::block_on(RunWorkspace::materialize(
            root.path(),
            ID,
            SHA,
            &[("a.rs".into(), b"one\n".to_vec())],
        ))
        .unwrap();
        ws.write_file("a.rs", b"one\ntwo\n").unwrap();
        tokio_test::block_on(ws.checkpoint()).unwrap();
        drop(ws);

        let ws = RunWorkspace::open_existing(root.path(), ID).unwrap();
        assert!(!tokio_test::block_on(ws.has_unexpected_changes()).unwrap());
        // Foreign edit after checkpoint.
        std::fs::write(ws.root().join("a.rs"), b"evil\n").unwrap();
        assert!(tokio_test::block_on(ws.has_unexpected_changes()).unwrap());
    }
}
