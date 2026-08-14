//! Worktree manager: the core module behind the WorktreeList UI.
//!
//! Owns the "which repository are we orchestrating" state, creates/removes worktrees
//! through the [`GitProvider`] boundary, persists branch rows (branch state survives
//! worktree re-creation), and publishes `worktree.*` events on the bus.

mod git_cli;
mod provider;

pub use git_cli::GitCli;
pub use provider::{
    BlameLine, BranchStatus, ChangedFile, GitProvider, LogEntry, MergeOutcome, Snapshot,
    WorktreeEntry,
};

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::core::bus::{Event, EventBus};
use crate::core::store::Store;
use crate::error::{GitErrorKind, MaestroError, Result};

/// Settings keys used by the manager.
pub const SETTING_REPO_PATH: &str = "repo_path";
pub const SETTING_BRANCH_TEMPLATE: &str = "branch_naming";

/// Where worktrees are created. Empty (the default) means beside the repository, in
/// `<parent>/<repo-name>.worktrees`. Set it when that directory is inconvenient — a work
/// repository on a managed path, a different disk, somewhere outside a backup sweep.
pub const SETTING_WORKTREE_ROOT: &str = "worktree_root";

/// Branch naming convention; configurable via the `branch_naming` setting.
pub const DEFAULT_BRANCH_TEMPLATE: &str = "{type}/{task-id}-{slug}";

/// The repository being orchestrated, as shown to the frontend.
#[derive(Clone, Debug, Serialize)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub default_branch: String,
    /// Local branches (valid targets for attach-existing).
    pub branches: Vec<String>,
    /// Remote-tracking branches (valid as base for new branches).
    pub remote_branches: Vec<String>,
}

/// A worktree row for the UI: git state merged with stored branch state.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeInfo {
    pub branch: Option<String>,
    pub path: PathBuf,
    pub is_primary: bool,
    pub task_id: Option<String>,
    pub base_branch: Option<String>,
    /// Kept at the top of the list regardless of sort order.
    pub pinned: bool,
    /// `None` when status could not be read (e.g. the directory vanished).
    pub status: Option<BranchStatus>,
    /// Newest commit's subject line — "what happened here last" at a glance.
    pub last_commit_subject: Option<String>,
}

/// Request to create (or re-attach) a worktree.
#[derive(Clone, Debug, Deserialize)]
pub struct CreateWorktreeRequest {
    /// Attach an existing branch instead of creating a new one.
    pub existing_branch: Option<String>,
    /// `{type}` component of the naming convention (e.g. "impl", "research").
    pub kind: Option<String>,
    pub task_id: Option<String>,
    pub slug: Option<String>,
    /// Base branch for a new branch; defaults to the repo's default branch.
    pub base: Option<String>,
}

/// Result of a remove request. A dirty worktree is not an error — it is a normal
/// state that requires explicit user confirmation (then `force = true`).
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RemoveOutcome {
    Removed,
    DirtyConfirmationRequired,
}

/// A merge outcome plus what the manager did to host it. Flattened so the
/// frontend sees one object: `{ merged, conflicts, message, switched_primary }`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MergeReport {
    #[serde(flatten)]
    pub outcome: MergeOutcome,
    /// True when the primary worktree was switched to the target branch to host
    /// the merge (the target was not checked out anywhere).
    pub switched_primary: bool,
    /// True when the primary was switched back to its original branch after a
    /// clean merge — the editor open on the primary never sees a branch change.
    pub switched_back: bool,
    /// Label of the snapshot holding the target worktree's uncommitted changes
    /// when they could not be restored automatically (merge conflict, or the
    /// restore itself conflicted). `None` means nothing is parked.
    pub parked_changes: Option<String>,
    /// True when the target's uncommitted changes were parked and then put
    /// back on top of the merged result — the seamless path.
    pub restored: bool,
}

/// Result of a snapshot-restore request: restoring over uncommitted changes
/// discards them, so a dirty worktree asks for confirmation first (same shape
/// as [`RemoveOutcome`]).
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreOutcome {
    Restored,
    DirtyConfirmationRequired,
}

pub struct WorktreeManager {
    git: Arc<dyn GitProvider>,
    store: Arc<dyn Store>,
    bus: EventBus,
    repo: RwLock<Option<PathBuf>>,
}

impl WorktreeManager {
    pub fn new(git: Arc<dyn GitProvider>, store: Arc<dyn Store>, bus: EventBus) -> Self {
        Self {
            git,
            store,
            bus,
            repo: RwLock::new(None),
        }
    }

    /// Restore the persisted repository path at startup, if it is still a git repo.
    pub fn load_persisted_repo(&self) -> Result<Option<PathBuf>> {
        let Some(saved) = self.store.get_setting(SETTING_REPO_PATH)? else {
            return Ok(None);
        };
        let path = PathBuf::from(&saved);
        if self.git.is_git_repo(&path)? {
            *self.write_repo()? = Some(path.clone());
            Ok(Some(path))
        } else {
            tracing::warn!(path = %saved, "persisted repo path is no longer a git repository");
            Ok(None)
        }
    }

    /// Select the repository to orchestrate; validated and persisted.
    pub fn set_repo(&self, path: &Path) -> Result<RepoInfo> {
        if !self.git.is_git_repo(path)? {
            return Err(MaestroError::Git {
                kind: GitErrorKind::NotARepository,
                message: format!("not a git repository: {}", path.display()),
            });
        }
        self.store
            .set_setting(SETTING_REPO_PATH, &path.to_string_lossy())?;
        *self.write_repo()? = Some(path.to_path_buf());
        tracing::info!(repo = %path.display(), "repository selected");
        self.repo_info_for(path)
    }

    /// Info about the currently selected repository, or `None` when unset.
    pub fn repo_info(&self) -> Result<Option<RepoInfo>> {
        match self.repo_path()? {
            Some(path) => self.repo_info_for(&path).map(Some),
            None => Ok(None),
        }
    }

    /// Worktrees of the selected repository, merged with stored branch state.
    pub fn list(&self) -> Result<Vec<WorktreeInfo>> {
        let repo = self.require_repo()?;
        let entries = self.git.list_worktrees(&repo)?;
        let mut infos = Vec::with_capacity(entries.len());
        for entry in entries {
            let stored = match &entry.branch {
                Some(branch) => self.store.get_branch(branch)?,
                None => None,
            };
            let status = match self.git.branch_status(&entry.path) {
                Ok(status) => Some(status),
                Err(err) => {
                    tracing::warn!(path = %entry.path.display(), error = %err, "status failed");
                    None
                }
            };
            let last_commit_subject = self.git.last_commit_subject(&entry.path).unwrap_or(None);
            infos.push(WorktreeInfo {
                branch: entry.branch,
                path: entry.path,
                is_primary: entry.is_primary,
                task_id: stored.as_ref().and_then(|b| b.task_id.clone()),
                base_branch: stored.as_ref().and_then(|b| b.base_branch.clone()),
                pinned: stored.as_ref().is_some_and(|b| b.pinned),
                status,
                last_commit_subject,
            });
        }
        Ok(infos)
    }

    /// Create a worktree. For a new branch the name follows the naming convention;
    /// for an existing branch the worktree is attached and stored state reattaches.
    pub fn create(&self, request: CreateWorktreeRequest) -> Result<WorktreeInfo> {
        let repo = self.require_repo()?;

        let (branch, is_new) = match &request.existing_branch {
            Some(branch) => {
                let branch = branch.trim();
                if !self.git.branch_exists(&repo, branch)? {
                    return Err(MaestroError::Git {
                        kind: GitErrorKind::InvalidInput,
                        message: format!("branch does not exist: {branch}"),
                    });
                }
                (branch.to_string(), false)
            }
            None => {
                let template = self
                    .store
                    .get_setting(SETTING_BRANCH_TEMPLATE)?
                    .unwrap_or_else(|| DEFAULT_BRANCH_TEMPLATE.to_string());
                let branch = render_branch_name(
                    &template,
                    request.kind.as_deref().unwrap_or("impl"),
                    request.task_id.as_deref().unwrap_or_default(),
                    request.slug.as_deref().unwrap_or_default(),
                )?;
                if self.git.branch_exists(&repo, &branch)? {
                    return Err(MaestroError::Git {
                        kind: GitErrorKind::InvalidInput,
                        message: format!(
                            "branch already exists: {branch} — attach it as an existing branch instead"
                        ),
                    });
                }
                (branch, true)
            }
        };

        let configured_root = self.store.get_setting(SETTING_WORKTREE_ROOT)?;
        let path = worktree_path(&repo, &branch, configured_root.as_deref());
        if path.exists() {
            return Err(MaestroError::InvalidData {
                message: format!("worktree path already exists: {}", path.display()),
            });
        }

        let base = if is_new {
            Some(match request.base.clone() {
                Some(base) => base,
                None => self.git.default_branch(&repo)?,
            })
        } else {
            None
        };

        self.git
            .create_worktree(&repo, &path, &branch, base.as_deref())?;

        // Persist branch state. COALESCE semantics in the store keep existing
        // task_id/base when re-attaching without new values.
        self.store
            .upsert_branch(&branch, request.task_id.as_deref(), base.as_deref())?;

        self.bus.publish(Event::WorktreeCreated {
            branch: branch.clone(),
            path: path.to_string_lossy().into_owned(),
        });
        tracing::info!(branch, path = %path.display(), new_branch = is_new, "worktree created");

        let stored = self.store.get_branch(&branch)?;
        let status = self.git.branch_status(&path).ok();
        let last_commit_subject = self.git.last_commit_subject(&path).unwrap_or(None);
        Ok(WorktreeInfo {
            branch: Some(branch),
            path,
            is_primary: false,
            task_id: stored.as_ref().and_then(|b| b.task_id.clone()),
            base_branch: stored.as_ref().and_then(|b| b.base_branch.clone()),
            pinned: stored.as_ref().is_some_and(|b| b.pinned),
            status,
            last_commit_subject,
        })
    }

    /// Get-or-create a worktree on an exactly-named branch, creating the branch
    /// from `base` when it doesn't exist yet. For system flows that own their
    /// naming (red-team worktrees, …) instead of the user-facing convention —
    /// deterministic names make the operation idempotent: calling it again
    /// returns the same worktree instead of minting a sibling.
    pub fn ensure_named(&self, name: &str, base: &str) -> Result<WorktreeInfo> {
        validate_branch_name(name)?;
        let repo = self.require_repo()?;

        // Already checked out somewhere → reuse it.
        if let Some(existing) = self
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(name))
        {
            return Ok(existing);
        }

        // Branch survived a worktree removal → reattach. Otherwise branch off `base`.
        if self.git.branch_exists(&repo, name)? {
            return self.create(CreateWorktreeRequest {
                existing_branch: Some(name.to_string()),
                kind: None,
                task_id: None,
                slug: None,
                base: None,
            });
        }

        let configured_root = self.store.get_setting(SETTING_WORKTREE_ROOT)?;
        let path = worktree_path(&repo, name, configured_root.as_deref());
        if path.exists() {
            return Err(MaestroError::InvalidData {
                message: format!("worktree path already exists: {}", path.display()),
            });
        }
        self.git.create_worktree(&repo, &path, name, Some(base))?;
        self.store.upsert_branch(name, None, Some(base))?;
        self.bus.publish(Event::WorktreeCreated {
            branch: name.to_string(),
            path: path.to_string_lossy().into_owned(),
        });
        tracing::info!(branch = name, base, path = %path.display(), "system worktree created");

        let stored = self.store.get_branch(name)?;
        let status = self.git.branch_status(&path).ok();
        let last_commit_subject = self.git.last_commit_subject(&path).unwrap_or(None);
        Ok(WorktreeInfo {
            branch: Some(name.to_string()),
            path,
            is_primary: false,
            task_id: stored.as_ref().and_then(|b| b.task_id.clone()),
            base_branch: stored.as_ref().and_then(|b| b.base_branch.clone()),
            pinned: stored.as_ref().is_some_and(|b| b.pinned),
            status,
            last_commit_subject,
        })
    }

    /// Pin or unpin a branch — kept at the top of the worktree list regardless
    /// of sort order, for the ones a user is actively juggling among many.
    pub fn set_pinned(&self, branch: &str, pinned: bool) -> Result<()> {
        self.store.set_branch_pinned(branch, pinned)
    }

    /// Remove the worktree checked out on `branch`. Without `force`, a dirty tree
    /// returns [`RemoveOutcome::DirtyConfirmationRequired`] instead of removing.
    /// The branch row in the store is kept — branch state survives.
    pub fn remove(&self, branch: &str, force: bool) -> Result<RemoveOutcome> {
        let repo = self.require_repo()?;
        let entry = self
            .git
            .list_worktrees(&repo)?
            .into_iter()
            .find(|e| e.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;

        if entry.is_primary {
            return Err(MaestroError::InvalidData {
                message: "cannot remove the primary worktree".into(),
            });
        }

        if !force {
            let status = self.git.branch_status(&entry.path)?;
            if status.dirty {
                return Ok(RemoveOutcome::DirtyConfirmationRequired);
            }
        }

        self.git.remove_worktree(&repo, &entry.path, force)?;
        self.bus.publish(Event::WorktreeRemoved {
            branch: branch.to_string(),
        });
        tracing::info!(branch, "worktree removed");
        Ok(RemoveOutcome::Removed)
    }

    /// Merge `source_branch`'s commits into `target_branch` (`git merge --no-ff`).
    ///
    /// Where the merge runs depends on where the target lives:
    /// - Checked out in some worktree → the merge runs there.
    /// - Not checked out anywhere → the **primary worktree is switched to the
    ///   target branch** and hosts the merge. That is the point, not a side
    ///   effect: the primary is what the user's editor (Rider) has open, so the
    ///   merged result — or the conflict to resolve — is immediately visible in
    ///   it. `git switch` DWIM also covers targets that only exist as
    ///   remote-tracking branches.
    ///
    /// Whichever worktree hosts the merge must be clean — refused outright
    /// rather than mixing a merge (or a branch switch) into the user's own
    /// uncommitted work. A dirty *source* worktree is not checked here: that
    /// only means its uncommitted work won't be part of the merge, which the
    /// frontend surfaces as a heads-up, not a hard error.
    pub fn merge_into(&self, source_branch: &str, target_branch: &str) -> Result<MergeReport> {
        if source_branch == target_branch {
            return Err(MaestroError::InvalidData {
                message: "cannot merge a branch into itself".into(),
            });
        }
        let repo = self.require_repo()?;
        let worktrees = self.git.list_worktrees(&repo)?;

        let (host, switched_primary, primary_home) = match worktrees
            .iter()
            .find(|e| e.branch.as_deref() == Some(target_branch))
        {
            Some(target) => (target, false, None),
            None => {
                let primary = worktrees.iter().find(|e| e.is_primary).ok_or_else(|| {
                    MaestroError::InvalidData {
                        message: "repository has no primary worktree".into(),
                    }
                })?;
                self.require_clean(&primary.path, target_branch)?;
                self.git.switch_branch(&primary.path, target_branch)?;
                tracing::info!(target_branch, "primary worktree switched to host the merge");
                (primary, true, primary.branch.clone())
            }
        };

        // A dirty target no longer blocks the merge: its uncommitted state is
        // parked in a snapshot, the merge runs on a clean tree, and the state
        // comes back on top of the result. Rider (or any editor) open on the
        // target sees the merge land under it without losing a keystroke.
        let mut parked: Option<String> = None;
        if !switched_primary && self.git.branch_status(&host.path)?.dirty {
            let label = format!("pre-merge of {source_branch}");
            self.git.snapshot_push(&host.path, &label)?;
            self.git.discard_changes(&host.path)?;
            parked = Some(label);
            tracing::info!(target_branch, "uncommitted target changes parked");
        }

        let outcome = match self.git.merge_branch(&host.path, source_branch) {
            Ok(outcome) => outcome,
            Err(err) => {
                // The merge never started (bad ref, broken repo…): put the
                // parked state straight back so the tree is as the user left it.
                if let Some(label) = parked.as_deref() {
                    if let Err(restore_err) = self.restore_parked(&host.path, label) {
                        tracing::warn!(error = %restore_err, "could not unpark after failed merge");
                    }
                }
                return Err(err);
            }
        };

        let mut restored = false;
        if let Some(label) = parked.as_deref() {
            if outcome.merged {
                match self.restore_parked(&host.path, label) {
                    Ok(()) => {
                        restored = true;
                        tracing::info!(target_branch, "parked changes restored on top of merge");
                    }
                    Err(err) => {
                        // The snapshot stays; the user restores it from the
                        // Snapshots dialog once they have looked at the clash.
                        tracing::warn!(error = %err, "parked changes conflict with the merge — kept as snapshot");
                    }
                }
            }
        }

        // A clean merge on a borrowed primary hands the primary back: the
        // editor open on it never notices, and the target branch simply gained
        // a commit. A conflicted merge must stay checked out to be resolved.
        let mut switched_back = false;
        if let (true, Some(home), true) =
            (switched_primary, primary_home.as_deref(), outcome.merged)
        {
            match self.git.switch_branch(&host.path, home) {
                Ok(()) => {
                    switched_back = true;
                    tracing::info!(home, "primary worktree switched back after merge");
                }
                Err(err) => tracing::warn!(error = %err, "could not switch the primary back"),
            }
        }

        if outcome.merged {
            self.bus.publish(Event::WorktreeMerged {
                source: source_branch.to_string(),
                target: target_branch.to_string(),
            });
            tracing::info!(
                source_branch,
                target_branch,
                switched_primary,
                "worktree merged"
            );
        } else {
            tracing::warn!(
                source_branch,
                target_branch,
                conflicts = outcome.conflicts.len(),
                "merge stopped short"
            );
        }
        Ok(MergeReport {
            outcome,
            switched_primary,
            switched_back,
            parked_changes: parked.filter(|_| !restored),
            restored,
        })
    }

    /// Re-apply the snapshot named `label` (the newest one) and drop it. Any
    /// failure leaves the snapshot in place for a manual restore.
    fn restore_parked(&self, worktree: &Path, label: &str) -> Result<()> {
        let snapshot = self
            .git
            .snapshot_list(worktree)?
            .into_iter()
            .find(|s| s.label == label)
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("parked snapshot '{label}' disappeared"),
            })?;
        self.git.snapshot_restore(worktree, &snapshot.id)?;
        self.git.snapshot_drop(worktree, &snapshot.id)?;
        Ok(())
    }

    /// Bring `branch` up to date with its base: fetch the base's remote state
    /// (best effort) and merge the freshest base ref into `branch`'s worktree.
    /// The inverse direction of [`merge_into`] — base flows *into* the feature
    /// branch, the standard "my branch is behind develop" fix, one per worktree
    /// after something lands in the base ("rebase-all" in spirit; a merge in
    /// mechanics, so nothing is rewritten and a conflict stays an ordinary,
    /// recoverable merge conflict).
    pub fn sync_with_base(&self, branch: &str) -> Result<MergeReport> {
        let repo = self.require_repo()?;
        let base = match self.store.get_branch(branch)?.and_then(|b| b.base_branch) {
            Some(base) => base,
            None => self.git.default_branch(&repo)?,
        };
        let fresh = self.git.fresh_base_ref(&repo, &base);
        tracing::info!(branch, base = %fresh, "syncing worktree with its base");
        self.merge_into(&fresh, branch)
    }

    /// Stage everything and commit in `branch`'s worktree. This is the *user's*
    /// commit button — the agents' commits still go through the PreToolUse gate;
    /// a person clicking "commit" on their own diff needs no approval dialog.
    pub fn commit_all(&self, branch: &str, message: &str) -> Result<String> {
        let message = message.trim();
        if message.is_empty() {
            return Err(MaestroError::InvalidData {
                message: "commit message must not be empty".into(),
            });
        }
        let repo = self.require_repo()?;
        let entry = self
            .git
            .list_worktrees(&repo)?
            .into_iter()
            .find(|e| e.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        let summary = self.git.commit_all(&entry.path, message)?;
        tracing::info!(branch, %summary, "worktree committed");
        Ok(summary)
    }

    /// Rewrite `path`'s line endings in `branch`'s worktree to `eol` (`"lf"` or
    /// `"crlf"`) — the diff viewer's Rider-style line-ending picker. A direct,
    /// ungated file write: the same trust level as the user's own `commit_all`
    /// button, not an agent action. A no-op (no write at all) when the file
    /// already matches, so it never manufactures a phantom dirty state.
    ///
    /// Works on raw bytes, not `String`: a real-world file (an old C# service,
    /// say) can carry non-UTF-8 bytes — a codepage-encoded string literal, a
    /// stray byte from years of edits — and `\r`/`\n` are the same single byte
    /// in every ASCII-compatible encoding regardless. Reading it as `String`
    /// would refuse a legitimately non-UTF-8 file outright; rewriting it
    /// through a lossy `String` round-trip would silently corrupt whatever
    /// wasn't valid UTF-8 by replacing it with U+FFFD. Neither is acceptable
    /// for a file mutation.
    pub fn set_line_ending(&self, branch: &str, path: &str, eol: &str) -> Result<()> {
        if eol != "lf" && eol != "crlf" {
            return Err(MaestroError::InvalidData {
                message: format!("unsupported line ending: {eol} (expected \"lf\" or \"crlf\")"),
            });
        }
        if path.split(['/', '\\']).any(|part| part == "..") {
            return Err(MaestroError::InvalidData {
                message: format!("invalid path: {path}"),
            });
        }
        let worktree = self.worktree_path(branch)?;
        let file_path = worktree.join(path);
        let content = std::fs::read(&file_path).map_err(|err| MaestroError::Config {
            message: format!("could not read {path}: {err}"),
        })?;
        let lf = strip_cr_before_lf(&content);
        let converted = if eol == "crlf" {
            expand_lf_to_crlf(&lf)
        } else {
            lf
        };
        if converted != content {
            std::fs::write(&file_path, &converted).map_err(|err| MaestroError::Config {
                message: format!("could not write {path}: {err}"),
            })?;
            tracing::info!(branch, path, eol, "line endings converted");
        }
        Ok(())
    }

    /// Record `branch`'s current uncommitted state as a named snapshot, leaving
    /// the working tree untouched — a checkpoint to fall back to when an agent's
    /// next attempt makes things worse instead of better.
    pub fn snapshot_take(&self, branch: &str, label: &str) -> Result<()> {
        let label = label.trim();
        let label = if label.is_empty() {
            "checkpoint"
        } else {
            label
        };
        let path = self.worktree_path(branch)?;
        self.git.snapshot_push(&path, label)?;
        tracing::info!(branch, label, "worktree snapshot taken");
        Ok(())
    }

    /// Snapshots of `branch`'s worktree, newest first.
    pub fn snapshot_list(&self, branch: &str) -> Result<Vec<Snapshot>> {
        let path = self.worktree_path(branch)?;
        self.git.snapshot_list(&path)
    }

    /// Replace the worktree's uncommitted state with snapshot `id`. Discards
    /// whatever is there now — a dirty worktree requires explicit confirmation
    /// (`confirmed = true`), a clean one has nothing to lose.
    pub fn snapshot_restore(
        &self,
        branch: &str,
        id: &str,
        confirmed: bool,
    ) -> Result<RestoreOutcome> {
        let path = self.worktree_path(branch)?;
        if !confirmed && self.git.branch_status(&path)?.dirty {
            return Ok(RestoreOutcome::DirtyConfirmationRequired);
        }
        self.git.snapshot_restore(&path, id)?;
        tracing::info!(branch, id, "worktree snapshot restored");
        Ok(RestoreOutcome::Restored)
    }

    /// Delete snapshot `id` of `branch`'s worktree.
    pub fn snapshot_drop(&self, branch: &str, id: &str) -> Result<()> {
        let path = self.worktree_path(branch)?;
        self.git.snapshot_drop(&path, id)?;
        tracing::info!(branch, id, "worktree snapshot dropped");
        Ok(())
    }

    /// Push `branch` to its remote. Only reachable through the explicit,
    /// user-confirmed Push dialog — agents' pushes still stop at the gate.
    pub fn push(&self, branch: &str) -> Result<String> {
        let path = self.worktree_path(branch)?;
        let report = self.git.push_branch(&path, branch)?;
        tracing::info!(branch, "branch pushed");
        Ok(report)
    }

    /// Update `branch` from origin without checking it out — used by the
    /// daemon to materialize a PR branch before creating its review worktree.
    pub fn fetch_branch(&self, branch: &str) -> Result<()> {
        let repo = self.require_repo()?;
        self.git.fetch_branch(&repo, branch)
    }

    /// Commits on `branch` that its (freshly fetched) base does not have —
    /// "what exactly is on this branch", the review aid before merge/push.
    pub fn branch_log(&self, branch: &str, limit: usize) -> Result<Vec<LogEntry>> {
        let repo = self.require_repo()?;
        let base = match self.store.get_branch(branch)?.and_then(|b| b.base_branch) {
            Some(base) => base,
            None => self.git.default_branch(&repo)?,
        };
        let fresh = self.git.fresh_base_ref(&repo, &base);
        self.git.branch_log(&repo, branch, &fresh, limit)
    }

    /// The path of the worktree that has `branch` checked out.
    fn worktree_path(&self, branch: &str) -> Result<PathBuf> {
        let repo = self.require_repo()?;
        self.git
            .list_worktrees(&repo)?
            .into_iter()
            .find(|e| e.branch.as_deref() == Some(branch))
            .map(|e| e.path)
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })
    }

    /// Refuse to touch a worktree that has uncommitted changes.
    fn require_clean(&self, worktree: &Path, label: &str) -> Result<()> {
        let status = self.git.branch_status(worktree)?;
        if status.dirty {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "the worktree that would host the merge into '{label}' has uncommitted \
                     changes — commit or discard them there first"
                ),
            });
        }
        Ok(())
    }

    fn repo_info_for(&self, path: &Path) -> Result<RepoInfo> {
        Ok(RepoInfo {
            path: path.to_path_buf(),
            default_branch: self.git.default_branch(path)?,
            branches: self.git.list_branches(path)?,
            remote_branches: self.git.list_remote_branches(path)?,
        })
    }

    fn repo_path(&self) -> Result<Option<PathBuf>> {
        Ok(self.repo.read().map_err(|_| lock_poisoned())?.clone())
    }

    fn require_repo(&self) -> Result<PathBuf> {
        self.repo_path()?.ok_or_else(|| MaestroError::Config {
            message: "no repository selected".into(),
        })
    }

    fn write_repo(&self) -> Result<std::sync::RwLockWriteGuard<'_, Option<PathBuf>>> {
        self.repo.write().map_err(|_| lock_poisoned())
    }
}

fn lock_poisoned() -> MaestroError {
    MaestroError::InvalidData {
        message: "worktree manager lock poisoned".into(),
    }
}

/// Where worktrees live. By default a sibling directory of the repo,
/// `<parent>/<repo-name>.worktrees/<branch-with-dashes>`; with `worktree_root` configured,
/// `<root>/<repo-name>/<branch-with-dashes>` — the repo name stays in the path so one root
/// can hold the worktrees of several repositories without collisions.
fn worktree_path(repo: &Path, branch: &str, configured_root: Option<&str>) -> PathBuf {
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let root = match configured_root.map(str::trim).filter(|r| !r.is_empty()) {
        Some(root) => PathBuf::from(root).join(&repo_name),
        None => repo
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.to_path_buf())
            .join(format!("{repo_name}.worktrees")),
    };
    root.join(branch.replace('/', "-"))
}

/// Drop the `\r` of every `\r\n` pair, byte-for-byte. A lone `\r` (old
/// Mac-style, not followed by `\n`) is left alone — same rule as the diff
/// engine's own `normalize_line_endings`.
fn strip_cr_before_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1; // skip the \r; the \n is pushed on the next loop turn
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Insert a `\r` before every `\n`. Applied to already-LF-normalized bytes,
/// so this is exactly the CRLF form regardless of what the input started as.
fn expand_lf_to_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// Render the branch naming convention. Unknown placeholders are left verbatim so a
/// misconfigured template is visible rather than silently swallowed.
pub fn render_branch_name(template: &str, kind: &str, task_id: &str, slug: &str) -> Result<String> {
    let kind = slugify(kind);
    let task_id = sanitize_component(task_id);
    let slug = slugify(slug);
    if task_id.is_empty() || slug.is_empty() {
        return Err(MaestroError::Git {
            kind: GitErrorKind::InvalidInput,
            message: "task id and slug are required to name a new branch".into(),
        });
    }
    let name = template
        .replace("{type}", &kind)
        .replace("{task-id}", &task_id)
        .replace("{slug}", &slug);
    validate_branch_name(&name)?;
    Ok(name)
}

/// Lowercase, alphanumerics and dashes only, collapsed.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true; // suppress leading dashes
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Task ids keep their case and dots/underscores (e.g. "PROJ-123").
fn sanitize_component(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

fn validate_branch_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(())
    } else {
        Err(MaestroError::Git {
            kind: GitErrorKind::InvalidInput,
            message: format!("invalid branch name: {name}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Add OAuth  Login!"), "add-oauth-login");
        assert_eq!(slugify("--weird--"), "weird");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn renders_default_template() {
        let name =
            render_branch_name(DEFAULT_BRANCH_TEMPLATE, "impl", "T-42", "diff viewer").unwrap();
        assert_eq!(name, "impl/T-42-diff-viewer");
    }

    #[test]
    fn rejects_missing_components() {
        assert!(render_branch_name(DEFAULT_BRANCH_TEMPLATE, "impl", "", "x").is_err());
        assert!(render_branch_name(DEFAULT_BRANCH_TEMPLATE, "impl", "T-1", "").is_err());
    }

    #[test]
    fn strips_cr_before_lf_only() {
        assert_eq!(strip_cr_before_lf(b"a\r\nb\r\nc\r\n"), b"a\nb\nc\n");
        assert_eq!(
            strip_cr_before_lf(b"a\nb\n"),
            b"a\nb\n",
            "pure LF is untouched"
        );
        // A lone \r (old Mac-style, not followed by \n) is left exactly alone.
        assert_eq!(strip_cr_before_lf(b"a\rb\n"), b"a\rb\n");
        assert_eq!(strip_cr_before_lf(b""), b"");
        // Non-UTF-8 bytes pass through byte-for-byte, untouched.
        assert_eq!(strip_cr_before_lf(b"a\xFF\xFE\r\nb"), b"a\xFF\xFE\nb");
    }

    #[test]
    fn expands_every_lf_to_crlf() {
        assert_eq!(expand_lf_to_crlf(b"a\nb\nc\n"), b"a\r\nb\r\nc\r\n");
        assert_eq!(
            expand_lf_to_crlf(b"a\r\nb\r\n"),
            b"a\r\r\nb\r\r\n",
            "not idempotent on its own — callers strip first"
        );
        assert_eq!(expand_lf_to_crlf(b""), b"");
        assert_eq!(expand_lf_to_crlf(b"a\xFF\nb"), b"a\xFF\r\nb");
    }

    #[test]
    fn worktree_path_is_sibling_of_repo() {
        let path = worktree_path(Path::new("C:/work/myrepo"), "impl/T-1-x", None);
        assert_eq!(path, Path::new("C:/work/myrepo.worktrees/impl-T-1-x"));

        // A configured root keeps the repo name in the path, so two repositories can share
        // one root without their branches colliding.
        let configured = worktree_path(
            Path::new("C:/work/myrepo"),
            "impl/T-1-x",
            Some("D:/maestro-worktrees"),
        );
        assert_eq!(
            configured,
            Path::new("D:/maestro-worktrees/myrepo/impl-T-1-x")
        );
        // Blank or whitespace is "not configured", not a root at the filesystem's mercy.
        assert_eq!(
            worktree_path(Path::new("C:/work/myrepo"), "impl/T-1-x", Some("   ")),
            path
        );
    }

    /// In-memory GitProvider double for manager tests.
    #[derive(Default)]
    struct MockGit {
        state: Mutex<MockState>,
    }

    #[derive(Default)]
    struct MockState {
        branches: HashSet<String>,
        worktrees: Vec<WorktreeEntry>,
        dirty: HashSet<PathBuf>,
        merge_calls: Vec<(PathBuf, String)>,
        merge_outcome: Option<MergeOutcome>,
        switch_calls: Vec<(PathBuf, String)>,
        commit_calls: Vec<(PathBuf, String)>,
        snapshots: Vec<Snapshot>,
        restore_calls: Vec<(PathBuf, String)>,
        discard_calls: Vec<PathBuf>,
        push_calls: Vec<(PathBuf, String)>,
    }

    impl MockGit {
        fn with_repo() -> Self {
            let mock = MockGit::default();
            {
                let mut st = mock.state.lock().unwrap();
                st.branches.insert("main".into());
                st.worktrees.push(WorktreeEntry {
                    path: PathBuf::from("/repo"),
                    head: "abc".into(),
                    branch: Some("main".into()),
                    is_primary: true,
                });
            }
            mock
        }
    }

    impl GitProvider for MockGit {
        fn is_git_repo(&self, path: &Path) -> Result<bool> {
            Ok(path == Path::new("/repo"))
        }
        fn default_branch(&self, _repo: &Path) -> Result<String> {
            Ok("main".into())
        }
        fn list_branches(&self, _repo: &Path) -> Result<Vec<String>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .branches
                .iter()
                .cloned()
                .collect())
        }
        fn list_remote_branches(&self, _repo: &Path) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn branch_exists(&self, _repo: &Path, branch: &str) -> Result<bool> {
            Ok(self.state.lock().unwrap().branches.contains(branch))
        }
        fn list_worktrees(&self, _repo: &Path) -> Result<Vec<WorktreeEntry>> {
            Ok(self.state.lock().unwrap().worktrees.clone())
        }
        fn create_worktree(
            &self,
            _repo: &Path,
            path: &Path,
            branch: &str,
            _base: Option<&str>,
        ) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.branches.insert(branch.to_string());
            st.worktrees.push(WorktreeEntry {
                path: path.to_path_buf(),
                head: "def".into(),
                branch: Some(branch.to_string()),
                is_primary: false,
            });
            Ok(())
        }
        fn remove_worktree(&self, _repo: &Path, path: &Path, _force: bool) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.worktrees.retain(|e| e.path != path);
            Ok(())
        }
        fn branch_status(&self, worktree: &Path) -> Result<BranchStatus> {
            let dirty = self.state.lock().unwrap().dirty.contains(worktree);
            Ok(BranchStatus {
                dirty,
                ahead: 0,
                behind: 0,
            })
        }
        fn merge_base_diff(&self, _repo: &Path, _branch: &str, _base: &str) -> Result<String> {
            Ok(String::new())
        }
        fn merge_base(&self, _repo: &Path, _base: &str, _branch: &str) -> Result<String> {
            Ok("0000000000000000000000000000000000000000".into())
        }
        fn fresh_base_ref(&self, _repo: &Path, base: &str) -> String {
            base.to_string()
        }
        fn changed_files(
            &self,
            _repo: &Path,
            _branch: &str,
            _base: &str,
        ) -> Result<Vec<provider::ChangedFile>> {
            Ok(Vec::new())
        }
        fn show_file(&self, _repo: &Path, _rev: &str, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn worktree_diff(&self, _worktree: &Path, _merge_base: &str) -> Result<String> {
            Ok(String::new())
        }
        fn worktree_changed_files(
            &self,
            _worktree: &Path,
            _merge_base: &str,
        ) -> Result<Vec<provider::ChangedFile>> {
            Ok(Vec::new())
        }
        fn blame_range(
            &self,
            _worktree: &Path,
            _path: &str,
            _start: u32,
            _end: u32,
        ) -> Result<Vec<provider::BlameLine>> {
            Ok(Vec::new())
        }
        fn merge_branch(
            &self,
            target_worktree: &Path,
            source_branch: &str,
        ) -> Result<MergeOutcome> {
            let mut st = self.state.lock().unwrap();
            st.merge_calls
                .push((target_worktree.to_path_buf(), source_branch.to_string()));
            Ok(st.merge_outcome.clone().unwrap_or(MergeOutcome {
                merged: true,
                conflicts: Vec::new(),
                message: "Fast-forward".into(),
            }))
        }
        fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.switch_calls
                .push((worktree.to_path_buf(), branch.to_string()));
            // Mirror git: the worktree entry now has the new branch checked out.
            if let Some(entry) = st.worktrees.iter_mut().find(|e| e.path == worktree) {
                entry.branch = Some(branch.to_string());
            }
            Ok(())
        }
        fn commit_all(&self, worktree: &Path, message: &str) -> Result<String> {
            let mut st = self.state.lock().unwrap();
            st.commit_calls
                .push((worktree.to_path_buf(), message.to_string()));
            st.dirty.remove(worktree);
            Ok(format!("abc1234 {message}"))
        }
        fn snapshot_push(&self, worktree: &Path, label: &str) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            let id = format!("stash@{{{}}}", st.snapshots.len());
            st.snapshots.push(Snapshot {
                id,
                label: label.to_string(),
                created_at: "2026-08-08 00:00:00 +0000".into(),
            });
            let _ = worktree;
            Ok(())
        }
        fn snapshot_list(&self, _worktree: &Path) -> Result<Vec<Snapshot>> {
            Ok(self.state.lock().unwrap().snapshots.clone())
        }
        fn snapshot_restore(&self, worktree: &Path, id: &str) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.restore_calls
                .push((worktree.to_path_buf(), id.to_string()));
            st.dirty.remove(worktree);
            Ok(())
        }
        fn snapshot_drop(&self, _worktree: &Path, id: &str) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.snapshots.retain(|s| s.id != id);
            Ok(())
        }
        fn discard_changes(&self, worktree: &Path) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.discard_calls.push(worktree.to_path_buf());
            st.dirty.remove(worktree);
            Ok(())
        }
        fn push_branch(&self, worktree: &Path, branch: &str) -> Result<String> {
            let mut st = self.state.lock().unwrap();
            st.push_calls
                .push((worktree.to_path_buf(), branch.to_string()));
            Ok(format!(
                "branch '{branch}' set up to track 'origin/{branch}'"
            ))
        }
        fn branch_log(
            &self,
            _repo: &Path,
            branch: &str,
            base: &str,
            _limit: usize,
        ) -> Result<Vec<LogEntry>> {
            Ok(vec![LogEntry {
                sha: "abc1234".into(),
                subject: format!("work on {branch} over {base}"),
                author: "Mock".into(),
                date: "2026-08-08".into(),
            }])
        }
    }

    fn manager() -> (WorktreeManager, EventBus) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mgr = WorktreeManager::new(Arc::new(MockGit::with_repo()), store, bus.clone());
        mgr.set_repo(Path::new("/repo")).unwrap();
        (mgr, bus)
    }

    #[tokio::test]
    async fn create_emits_event_and_persists_branch() {
        let (mgr, bus) = manager();
        let mut rx = bus.subscribe();

        let info = mgr
            .create(CreateWorktreeRequest {
                existing_branch: None,
                kind: Some("impl".into()),
                task_id: Some("T-7".into()),
                slug: Some("gate flow".into()),
                base: None,
            })
            .unwrap();

        assert_eq!(info.branch.as_deref(), Some("impl/T-7-gate-flow"));
        assert_eq!(info.task_id.as_deref(), Some("T-7"));
        assert_eq!(info.base_branch.as_deref(), Some("main"));

        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "worktree.created");
    }

    #[tokio::test]
    async fn pinning_a_branch_surfaces_in_the_worktree_list() {
        let (mgr, _bus) = manager();
        let info = mgr
            .create(CreateWorktreeRequest {
                existing_branch: None,
                kind: Some("impl".into()),
                task_id: Some("T-8".into()),
                slug: Some("pin flow".into()),
                base: None,
            })
            .unwrap();
        let branch = info.branch.unwrap();
        assert!(!info.pinned, "unpinned by default");

        mgr.set_pinned(&branch, true).unwrap();
        let listed = mgr.list().unwrap();
        let found = listed.iter().find(|w| w.branch.as_deref() == Some(&branch));
        assert!(found.unwrap().pinned);

        mgr.set_pinned(&branch, false).unwrap();
        let listed = mgr.list().unwrap();
        let found = listed.iter().find(|w| w.branch.as_deref() == Some(&branch));
        assert!(!found.unwrap().pinned);
    }

    #[tokio::test]
    async fn reattaching_existing_branch_keeps_stored_state() {
        let (mgr, bus) = manager();

        mgr.create(CreateWorktreeRequest {
            existing_branch: None,
            kind: Some("impl".into()),
            task_id: Some("T-9".into()),
            slug: Some("attention".into()),
            base: None,
        })
        .unwrap();

        // Remove the worktree; the branch row must survive.
        assert_eq!(
            mgr.remove("impl/T-9-attention", false).unwrap(),
            RemoveOutcome::Removed
        );

        // Re-create for the existing branch without passing task metadata.
        let info = mgr
            .create(CreateWorktreeRequest {
                existing_branch: Some("impl/T-9-attention".into()),
                kind: None,
                task_id: None,
                slug: None,
                base: None,
            })
            .unwrap();

        assert_eq!(
            info.task_id.as_deref(),
            Some("T-9"),
            "stored state reattached"
        );
        drop(bus);
    }

    #[tokio::test]
    async fn dirty_worktree_requires_confirmation() {
        let (mgr, bus) = manager();
        let git = MockGit::with_repo();
        // Build a manager sharing the same mock so we can flip the dirty flag.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let git = Arc::new(git);
        let mgr2 = WorktreeManager::new(git.clone(), store, bus.clone());
        mgr2.set_repo(Path::new("/repo")).unwrap();

        let info = mgr2
            .create(CreateWorktreeRequest {
                existing_branch: None,
                kind: Some("impl".into()),
                task_id: Some("T-3".into()),
                slug: Some("chat".into()),
                base: None,
            })
            .unwrap();

        git.state.lock().unwrap().dirty.insert(info.path.clone());

        assert_eq!(
            mgr2.remove("impl/T-3-chat", false).unwrap(),
            RemoveOutcome::DirtyConfirmationRequired
        );
        assert_eq!(
            mgr2.remove("impl/T-3-chat", true).unwrap(),
            RemoveOutcome::Removed
        );
        drop(mgr);
    }

    #[test]
    fn remove_primary_is_rejected() {
        let (mgr, _bus) = manager();
        assert!(mgr.remove("main", false).is_err());
    }

    /// A manager plus a directly-held `Arc<MockGit>`, so a test can both drive
    /// `WorktreeManager` and reach into the mock's state (flip dirty, set the
    /// next merge outcome, inspect recorded calls).
    fn manager_with_git() -> (WorktreeManager, Arc<MockGit>, EventBus) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let git = Arc::new(MockGit::with_repo());
        let mgr = WorktreeManager::new(git.clone(), store, bus.clone());
        mgr.set_repo(Path::new("/repo")).unwrap();
        (mgr, git, bus)
    }

    #[test]
    fn merge_into_rejects_merging_a_branch_into_itself() {
        let (mgr, _git, _bus) = manager_with_git();
        let err = mgr.merge_into("main", "main").unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");
    }

    #[test]
    fn ensure_named_creates_once_then_reuses() {
        let (mgr, git, _bus) = manager_with_git();

        let first = mgr
            .ensure_named("redteam/impl-T-1-x", "impl/T-1-x")
            .unwrap();
        assert_eq!(first.branch.as_deref(), Some("redteam/impl-T-1-x"));
        assert_eq!(
            first.base_branch.as_deref(),
            Some("impl/T-1-x"),
            "the parent is recorded as the base"
        );
        assert!(git
            .state
            .lock()
            .unwrap()
            .branches
            .contains("redteam/impl-T-1-x"));

        // Second call finds the existing worktree instead of erroring on the path.
        let second = mgr
            .ensure_named("redteam/impl-T-1-x", "impl/T-1-x")
            .unwrap();
        assert_eq!(second.path, first.path);
        assert_eq!(
            git.state
                .lock()
                .unwrap()
                .worktrees
                .iter()
                .filter(|w| w.branch.as_deref() == Some("redteam/impl-T-1-x"))
                .count(),
            1,
            "no sibling worktree was minted"
        );
    }

    #[test]
    fn ensure_named_rejects_invalid_names() {
        let (mgr, _git, _bus) = manager_with_git();
        assert!(mgr.ensure_named("bad name", "main").is_err());
        assert!(mgr.ensure_named("also//bad", "main").is_err());
    }

    #[test]
    fn merge_into_parks_a_dirty_target_and_restores_it_afterwards() {
        let (mgr, git, _bus) = manager_with_git();
        git.state
            .lock()
            .unwrap()
            .dirty
            .insert(PathBuf::from("/repo"));

        let report = mgr.merge_into("impl/T-1-x", "main").unwrap();
        assert!(report.outcome.merged);
        assert!(report.restored, "the parked state came back");
        assert_eq!(report.parked_changes, None);

        let st = git.state.lock().unwrap();
        assert_eq!(st.discard_calls, vec![PathBuf::from("/repo")]);
        assert_eq!(
            st.merge_calls,
            vec![(PathBuf::from("/repo"), "impl/T-1-x".to_string())],
            "the merge ran on the cleaned tree"
        );
        assert_eq!(st.restore_calls.len(), 1, "the parked state was re-applied");
        assert!(
            st.snapshots.is_empty(),
            "the parking snapshot is dropped after a clean restore"
        );
    }

    #[tokio::test]
    async fn merge_into_runs_the_merge_and_publishes_on_success() {
        let (mgr, git, bus) = manager_with_git();
        let mut rx = bus.subscribe();

        let report = mgr.merge_into("impl/T-1-x", "main").unwrap();
        assert!(report.outcome.merged);
        assert!(!report.switched_primary, "'main' is already checked out");
        assert_eq!(
            git.state.lock().unwrap().merge_calls,
            vec![(PathBuf::from("/repo"), "impl/T-1-x".to_string())]
        );
        assert!(git.state.lock().unwrap().switch_calls.is_empty());

        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "worktree.merged");
    }

    /// The Rider path: a target that no worktree has checked out is hosted by
    /// the primary worktree, which is switched to it first.
    #[test]
    fn merge_into_an_unchecked_out_branch_switches_the_primary_first() {
        let (mgr, git, _bus) = manager_with_git();

        let report = mgr.merge_into("impl/T-1-x", "develop").unwrap();
        assert!(report.outcome.merged);
        assert!(report.switched_primary);
        assert!(report.switched_back, "the primary is returned afterwards");
        let st = git.state.lock().unwrap();
        assert_eq!(
            st.switch_calls,
            vec![
                (PathBuf::from("/repo"), "develop".to_string()),
                (PathBuf::from("/repo"), "main".to_string()),
            ],
            "switch to host the merge, then hand the primary back"
        );
        assert_eq!(
            st.merge_calls,
            vec![(PathBuf::from("/repo"), "impl/T-1-x".to_string())],
            "the merge runs in the freshly-switched primary"
        );
    }

    #[test]
    fn merge_into_an_unchecked_out_branch_refuses_a_dirty_primary() {
        let (mgr, git, _bus) = manager_with_git();
        git.state
            .lock()
            .unwrap()
            .dirty
            .insert(PathBuf::from("/repo"));

        let err = mgr.merge_into("impl/T-1-x", "develop").unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
        let st = git.state.lock().unwrap();
        assert!(
            st.switch_calls.is_empty() && st.merge_calls.is_empty(),
            "a dirty primary must not be switched, let alone merged into"
        );
    }

    #[test]
    fn sync_with_base_merges_the_stored_base_into_the_branch_worktree() {
        let (mgr, git, _bus) = manager_with_git();
        let info = mgr
            .create(CreateWorktreeRequest {
                existing_branch: None,
                kind: Some("impl".into()),
                task_id: Some("T-5".into()),
                slug: Some("sync".into()),
                base: None, // defaults to "main", stored as the branch's base
            })
            .unwrap();

        let report = mgr.sync_with_base("impl/T-5-sync").unwrap();
        assert!(report.outcome.merged);
        assert_eq!(
            git.state.lock().unwrap().merge_calls,
            vec![(info.path, "main".to_string())],
            "the base merges into the branch's own worktree"
        );
    }

    #[test]
    fn commit_all_commits_in_the_branch_worktree_and_rejects_empty_messages() {
        let (mgr, git, _bus) = manager_with_git();

        assert!(mgr.commit_all("main", "   ").is_err(), "blank message");
        assert!(
            mgr.commit_all("no-such-branch", "msg").is_err(),
            "unknown branch"
        );

        let summary = mgr.commit_all("main", "fix: the thing").unwrap();
        assert!(summary.contains("fix: the thing"));
        assert_eq!(
            git.state.lock().unwrap().commit_calls,
            vec![(PathBuf::from("/repo"), "fix: the thing".to_string())]
        );
    }

    #[test]
    fn snapshot_restore_over_a_dirty_worktree_needs_confirmation() {
        let (mgr, git, _bus) = manager_with_git();
        mgr.snapshot_take("main", "before the risky attempt")
            .unwrap();
        git.state
            .lock()
            .unwrap()
            .dirty
            .insert(PathBuf::from("/repo"));

        let snapshots = mgr.snapshot_list("main").unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].label, "before the risky attempt");

        let id = snapshots[0].id.clone();
        assert_eq!(
            mgr.snapshot_restore("main", &id, false).unwrap(),
            RestoreOutcome::DirtyConfirmationRequired,
            "dirty state is not silently discarded"
        );
        assert!(git.state.lock().unwrap().restore_calls.is_empty());

        assert_eq!(
            mgr.snapshot_restore("main", &id, true).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(
            git.state.lock().unwrap().restore_calls,
            vec![(PathBuf::from("/repo"), id)]
        );

        mgr.snapshot_drop("main", &snapshots[0].id).unwrap();
        assert!(mgr.snapshot_list("main").unwrap().is_empty());
    }

    #[test]
    fn push_runs_in_the_branch_worktree_and_branch_log_reads_the_stored_base() {
        let (mgr, git, _bus) = manager_with_git();

        let report = mgr.push("main").unwrap();
        assert!(report.contains("origin/main"));
        assert_eq!(
            git.state.lock().unwrap().push_calls,
            vec![(PathBuf::from("/repo"), "main".to_string())]
        );
        assert!(mgr.push("no-such-branch").is_err());

        let log = mgr.branch_log("main", 50).unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].subject.contains("main"));
    }

    #[test]
    fn merge_into_reports_conflicts_without_pretending_to_have_merged() {
        let (mgr, git, bus) = manager_with_git();
        let mut rx = bus.subscribe();
        git.state.lock().unwrap().merge_outcome = Some(MergeOutcome {
            merged: false,
            conflicts: vec!["src/lib.rs".into()],
            message: "CONFLICT (content): Merge conflict in src/lib.rs".into(),
        });

        let report = mgr.merge_into("impl/T-1-x", "main").unwrap();
        assert!(!report.outcome.merged);
        assert_eq!(report.outcome.conflicts, vec!["src/lib.rs".to_string()]);
        assert!(
            rx.try_recv().is_err(),
            "a conflicted merge must not publish worktree.merged"
        );
    }

    // ---- real-git integration: the seamless merge behaviors ----

    mod seamless_merge {
        use super::*;
        use std::fs;
        use std::process::Command;

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn init_repo(dir: &Path) {
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "t@t.t"]);
            git(dir, &["config", "user.name", "t"]);
            fs::write(dir.join("a.txt"), "base\n").unwrap();
            git(dir, &["add", "-A"]);
            git(dir, &["commit", "-m", "init"]);
        }

        fn manager_on(repo: &Path) -> WorktreeManager {
            let store = Arc::new(SqliteStore::open_in_memory().unwrap());
            let mgr = WorktreeManager::new(Arc::new(GitCli::new()), store, EventBus::new());
            mgr.set_repo(repo).unwrap();
            mgr
        }

        fn feature_worktree(mgr: &WorktreeManager) -> String {
            let info = mgr
                .create(CreateWorktreeRequest {
                    existing_branch: None,
                    kind: Some("impl".into()),
                    task_id: Some("T-1".into()),
                    slug: Some("merge test".into()),
                    base: Some("main".into()),
                })
                .unwrap();
            info.branch.unwrap()
        }

        #[test]
        fn a_dirty_target_is_parked_merged_and_restored_seamlessly() {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            init_repo(&repo);
            let mgr = manager_on(&repo);

            let feature = feature_worktree(&mgr);
            let wt = repo
                .parent()
                .unwrap()
                .join("repo.worktrees")
                .join("impl-T-1-merge-test");
            fs::write(wt.join("b.txt"), "from branch\n").unwrap();
            mgr.commit_all(&feature, "add b").unwrap();

            // Rider-style situation: uncommitted work sitting in the target.
            fs::write(repo.join("wip.txt"), "uncommitted work\n").unwrap();

            let report = mgr.merge_into(&feature, "main").unwrap();
            assert!(report.outcome.merged, "{}", report.outcome.message);
            assert!(report.restored, "uncommitted state came back");
            assert_eq!(report.parked_changes, None);
            assert_eq!(
                fs::read_to_string(repo.join("b.txt"))
                    .unwrap()
                    .replace("\r\n", "\n"),
                "from branch\n",
                "the merge landed"
            );
            assert_eq!(
                fs::read_to_string(repo.join("wip.txt"))
                    .unwrap()
                    .replace("\r\n", "\n"),
                "uncommitted work\n",
                "the WIP file survived the merge untouched"
            );
            assert!(
                mgr.snapshot_list("main").unwrap().is_empty(),
                "the parking snapshot is cleaned up after a seamless restore"
            );
        }

        #[test]
        fn a_restore_clash_keeps_the_parked_snapshot_instead_of_guessing() {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            init_repo(&repo);
            let mgr = manager_on(&repo);

            let feature = feature_worktree(&mgr);
            let wt = repo
                .parent()
                .unwrap()
                .join("repo.worktrees")
                .join("impl-T-1-merge-test");
            fs::write(wt.join("a.txt"), "branch version\n").unwrap();
            mgr.commit_all(&feature, "change a").unwrap();

            // Local uncommitted edit to the same file the branch rewrites.
            fs::write(repo.join("a.txt"), "local uncommitted version\n").unwrap();

            let report = mgr.merge_into(&feature, "main").unwrap();
            assert!(report.outcome.merged);
            assert!(!report.restored);
            let label = report.parked_changes.expect("changes stay parked");
            let snapshots = mgr.snapshot_list("main").unwrap();
            assert!(
                snapshots.iter().any(|s| s.label == label),
                "the snapshot named in the report actually exists"
            );
        }

        #[test]
        fn merging_into_an_uncheckedout_branch_returns_the_primary_afterwards() {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            init_repo(&repo);
            git(&repo, &["branch", "develop"]);
            let mgr = manager_on(&repo);

            let feature = feature_worktree(&mgr);
            let wt = repo
                .parent()
                .unwrap()
                .join("repo.worktrees")
                .join("impl-T-1-merge-test");
            fs::write(wt.join("b.txt"), "from branch\n").unwrap();
            mgr.commit_all(&feature, "add b").unwrap();

            let report = mgr.merge_into(&feature, "develop").unwrap();
            assert!(report.outcome.merged);
            assert!(report.switched_primary);
            assert!(report.switched_back, "the primary is handed back");

            let head = Command::new("git")
                .args(["branch", "--show-current"])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&head.stdout).trim(),
                "main",
                "the primary worktree is back on its own branch"
            );
            let merged_file = Command::new("git")
                .args(["show", "develop:b.txt"])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                merged_file.status.success(),
                "develop received the merge without ever needing a worktree"
            );
        }
    }

    mod line_endings {
        use super::*;
        use std::fs;
        use std::process::Command;

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn init_repo(dir: &Path) {
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "t@t.t"]);
            git(dir, &["config", "user.name", "t"]);
            fs::write(dir.join("a.txt"), "base\n").unwrap();
            git(dir, &["add", "-A"]);
            git(dir, &["commit", "-m", "init"]);
        }

        fn setup() -> (WorktreeManager, PathBuf, String, tempfile::TempDir) {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            init_repo(&repo);

            let store = Arc::new(SqliteStore::open_in_memory().unwrap());
            let mgr = WorktreeManager::new(Arc::new(GitCli::new()), store, EventBus::new());
            mgr.set_repo(&repo).unwrap();
            let info = mgr
                .create(CreateWorktreeRequest {
                    existing_branch: None,
                    kind: Some("impl".into()),
                    task_id: Some("T-1".into()),
                    slug: Some("eol test".into()),
                    base: Some("main".into()),
                })
                .unwrap();
            let branch = info.branch.unwrap();
            (mgr, info.path, branch, tmp)
        }

        #[test]
        fn converts_lf_to_crlf_and_back() {
            let (mgr, wt, branch, _tmp) = setup();
            fs::write(wt.join("service.cs"), "line one\nline two\n").unwrap();

            mgr.set_line_ending(&branch, "service.cs", "crlf").unwrap();
            assert_eq!(
                fs::read_to_string(wt.join("service.cs")).unwrap(),
                "line one\r\nline two\r\n"
            );

            mgr.set_line_ending(&branch, "service.cs", "lf").unwrap();
            assert_eq!(
                fs::read_to_string(wt.join("service.cs")).unwrap(),
                "line one\nline two\n"
            );
        }

        #[test]
        fn normalizes_mixed_endings_to_the_requested_style() {
            let (mgr, wt, branch, _tmp) = setup();
            fs::write(
                wt.join("service.cs"),
                "line one\r\nline two\nline three\r\n",
            )
            .unwrap();

            mgr.set_line_ending(&branch, "service.cs", "lf").unwrap();
            assert_eq!(
                fs::read_to_string(wt.join("service.cs")).unwrap(),
                "line one\nline two\nline three\n"
            );
        }

        #[test]
        fn is_a_true_no_op_when_already_in_the_requested_style() {
            let (mgr, wt, branch, _tmp) = setup();
            let path = wt.join("service.cs");
            fs::write(&path, "line one\nline two\n").unwrap();
            let before = fs::metadata(&path).unwrap().modified().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));

            mgr.set_line_ending(&branch, "service.cs", "lf").unwrap();
            let after = fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(before, after, "an already-LF file must not be rewritten");
        }

        #[test]
        fn rejects_a_path_that_escapes_the_worktree() {
            let (mgr, _wt, branch, _tmp) = setup();
            let err = mgr
                .set_line_ending(&branch, "../../etc/passwd", "lf")
                .unwrap_err();
            assert!(err.to_string().contains("invalid path"));
        }

        #[test]
        fn rejects_an_unknown_style() {
            let (mgr, wt, branch, _tmp) = setup();
            fs::write(wt.join("service.cs"), "line one\n").unwrap();
            let err = mgr
                .set_line_ending(&branch, "service.cs", "cr")
                .unwrap_err();
            assert!(err.to_string().contains("unsupported"));
        }

        /// The real-world case: an old file with a codepage-encoded string
        /// literal (not valid UTF-8 at all). Byte-level conversion must not
        /// refuse it, and must not corrupt the non-UTF-8 bytes.
        #[test]
        fn converts_a_non_utf8_file_without_corrupting_it() {
            let (mgr, wt, branch, _tmp) = setup();
            let path = wt.join("service.cs");
            let mut content = b"// comment: \xC0\xE0\xE1\xEB\xE8\xF6\xE0\r\n".to_vec();
            content.extend_from_slice(b"fn ok() {}\r\n");
            fs::write(&path, &content).unwrap();

            mgr.set_line_ending(&branch, "service.cs", "lf").unwrap();
            let after = fs::read(&path).unwrap();
            let mut expected = b"// comment: \xC0\xE0\xE1\xEB\xE8\xF6\xE0\n".to_vec();
            expected.extend_from_slice(b"fn ok() {}\n");
            assert_eq!(
                after, expected,
                "the non-UTF-8 bytes must survive untouched"
            );
        }
    }
}
