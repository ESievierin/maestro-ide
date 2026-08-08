//! Git boundary. All core logic depends on [`GitProvider`], never on the git CLI
//! directly — this is what enables test doubles and a future libgit2/GitLab impl.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{GitErrorKind, MaestroError, Result};

/// One entry from `git worktree list`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    /// `None` when the worktree is on a detached HEAD.
    pub branch: Option<String>,
    /// The main working tree of the repository (cannot be removed).
    pub is_primary: bool,
}

/// Cheap status summary for a worktree's checked-out branch.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct BranchStatus {
    /// Uncommitted changes present (staged, unstaged, or untracked).
    pub dirty: bool,
    /// Commits ahead of upstream (0 when no upstream).
    pub ahead: u32,
    /// Commits behind upstream (0 when no upstream).
    pub behind: u32,
}

/// One file changed between a branch and its merge-base with the base branch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// Single-letter git status: A(dded), M(odified), D(eleted), R(enamed), …
    pub status: String,
    /// Original path for renames/copies.
    pub old_path: Option<String>,
}

/// One line of `git blame` output for a requested range.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BlameLine {
    pub sha: String,
    pub author: String,
    pub summary: String,
    /// 1-based line number in the current file.
    pub line: u32,
    pub content: String,
}

/// External git boundary. Synchronous by design — the git CLI is a blocking
/// subprocess; async callers wrap invocations in `spawn_blocking`.
pub trait GitProvider: Send + Sync {
    /// Is `path` inside a git working tree?
    fn is_git_repo(&self, path: &Path) -> Result<bool>;

    /// Best-effort default branch: origin/HEAD if set, else the currently
    /// checked-out branch of the primary worktree.
    fn default_branch(&self, repo: &Path) -> Result<String>;

    /// Local branch names.
    fn list_branches(&self, repo: &Path) -> Result<Vec<String>>;

    /// Remote-tracking branch names (e.g. `origin/main`), symbolic refs excluded.
    fn list_remote_branches(&self, repo: &Path) -> Result<Vec<String>>;

    fn branch_exists(&self, repo: &Path, branch: &str) -> Result<bool>;

    fn list_worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>>;

    /// Create a worktree at `path`. With `base = Some(_)` a new branch `branch` is
    /// created from `base`; with `base = None` the existing `branch` is checked out.
    fn create_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<()>;

    /// Remove the worktree at `path`. `force` discards uncommitted changes.
    fn remove_worktree(&self, repo: &Path, path: &Path, force: bool) -> Result<()>;

    /// Status of the branch checked out at `worktree`.
    fn branch_status(&self, worktree: &Path) -> Result<BranchStatus>;

    /// Raw unified diff of `branch` against its merge-base with `base`
    /// (`git diff base...branch`). Consumed by the diff engine in T5.
    fn merge_base_diff(&self, repo: &Path, branch: &str, base: &str) -> Result<String>;

    /// The merge-base commit of `base` and `branch`.
    fn merge_base(&self, repo: &Path, base: &str, branch: &str) -> Result<String>;

    /// Best-effort: bring `base`'s remote-tracking ref up to date with its remote
    /// (default `origin` when `base` is a plain branch name, not already
    /// `<remote>/<branch>`), then return the ref to actually diff against — the
    /// freshened remote-tracking ref when one exists, `base` unchanged otherwise.
    ///
    /// Never fails the caller: offline, no such remote, or a purely local repo all
    /// just fall back to whatever `base` already resolves to. Without this, a diff
    /// compares against whatever the local ref happened to point at whenever it was
    /// last fetched — in practice, often frozen at around worktree-creation time.
    fn fresh_base_ref(&self, repo: &Path, base: &str) -> String;

    /// Files changed between the merge-base and `branch` (rename detection on).
    fn changed_files(&self, repo: &Path, branch: &str, base: &str) -> Result<Vec<ChangedFile>>;

    /// Contents of `path` at `rev`; `None` when the file does not exist there.
    fn show_file(&self, repo: &Path, rev: &str, path: &str) -> Result<Option<String>>;

    /// Blame for lines `start..=end` of `path` in the given worktree (working tree
    /// state, so uncommitted lines blame to the "not committed" placeholder).
    fn blame_range(
        &self,
        worktree: &Path,
        path: &str,
        start: u32,
        end: u32,
    ) -> Result<Vec<BlameLine>>;

    /// Unified diff from `merge_base` to the **working tree** of `worktree`
    /// (includes uncommitted changes to tracked files).
    fn worktree_diff(&self, worktree: &Path, merge_base: &str) -> Result<String>;

    /// Files changed from `merge_base` to the working tree, plus untracked files
    /// (reported with status `A`).
    fn worktree_changed_files(&self, worktree: &Path, merge_base: &str)
        -> Result<Vec<ChangedFile>>;

    /// Merge `source_branch` into whatever is checked out at `target_worktree`
    /// (`git merge --no-ff --no-edit`). The caller must ensure `target_worktree`
    /// is clean first — this only runs the merge itself. A conflict is reported
    /// as `MergeOutcome { merged: false, .. }`, not an error: it is a normal,
    /// recoverable stopping point, and the working tree is left exactly as git
    /// itself leaves it (conflict markers in place, resolvable with any git tool).
    fn merge_branch(&self, target_worktree: &Path, source_branch: &str) -> Result<MergeOutcome>;

    /// Check `branch` out at `worktree` (`git switch`). Git's DWIM applies: a name
    /// that only exists as a remote-tracking branch gets a local tracking branch
    /// created. The caller must ensure `worktree` is clean first.
    fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()>;

    /// Bring `branch` up to date from origin without checking it out
    /// (`git fetch origin branch:branch`). When the branch is checked out in
    /// some worktree that refspec is refused — a plain `git fetch origin branch`
    /// is used instead, which is enough because a checkout means the branch
    /// already exists locally. Default: no-op for providers without a network.
    fn fetch_branch(&self, _repo: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    /// Throw away every uncommitted change in `worktree` (`reset --hard` +
    /// `clean -fd`). Callers park the state in a snapshot first — this is the
    /// second half of "stash, do something, restore".
    fn discard_changes(&self, _worktree: &Path) -> Result<()> {
        Err(MaestroError::Git {
            kind: GitErrorKind::CommandFailed,
            message: "discard_changes is not supported by this git provider".into(),
        })
    }

    /// Stage everything (`git add -A`) and commit with `message` in `worktree`.
    /// Returns the new commit's one-line summary (`<short-sha> <subject>`).
    /// Hook failures and "nothing to commit" surface as errors with git's own text.
    fn commit_all(&self, worktree: &Path, message: &str) -> Result<String>;

    /// Record the worktree's current uncommitted state (tracked + untracked) as
    /// a named snapshot without changing the working tree. Stash-backed: the
    /// snapshots survive app restarts and are visible to plain `git stash list`.
    /// Errors when there is nothing to snapshot.
    fn snapshot_push(&self, worktree: &Path, label: &str) -> Result<()>;

    /// Snapshots previously taken in `worktree`, newest first. Only entries
    /// created by [`snapshot_push`] are listed — the user's own stashes are not
    /// touched or shown.
    fn snapshot_list(&self, worktree: &Path) -> Result<Vec<Snapshot>>;

    /// Replace the working tree's uncommitted state with snapshot `id`:
    /// `reset --hard` + `clean -fd`, then apply the snapshot (kept for reuse).
    /// The caller confirms discarding the current state first.
    fn snapshot_restore(&self, worktree: &Path, id: &str) -> Result<()>;

    /// Delete snapshot `id`.
    fn snapshot_drop(&self, worktree: &Path, id: &str) -> Result<()>;

    /// Push `branch` to its remote (`git push -u origin <branch>`), returning
    /// git's own report. Only ever called from an explicit, user-confirmed
    /// action — agents' pushes go through the approval gate instead.
    fn push_branch(&self, worktree: &Path, branch: &str) -> Result<String>;

    /// Commits on `branch` that are not on `base` (`git log base..branch`),
    /// newest first, capped at `limit`.
    fn branch_log(
        &self,
        repo: &Path,
        branch: &str,
        base: &str,
        limit: usize,
    ) -> Result<Vec<LogEntry>>;
}

/// One commit line for the branch log view.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LogEntry {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub date: String,
}

/// One saved worktree snapshot (a specially-labeled git stash entry).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    /// Stash reference (`stash@{N}`). Positional — refresh the list before use.
    pub id: String,
    pub label: String,
    /// Git's committer date for the stash entry (ISO-ish, as git prints it).
    pub created_at: String,
}

/// Outcome of a merge attempt.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MergeOutcome {
    /// True if the merge completed (a fast-forward or a new merge commit).
    pub merged: bool,
    /// Paths with conflict markers, when `merged` is false because of a conflict.
    /// Empty (with `merged: false`) means git failed for some other reason —
    /// see `message`.
    pub conflicts: Vec<String>,
    /// Git's own stdout/stderr, shown verbatim so an unexpected failure is never
    /// silently swallowed.
    pub message: String,
}
