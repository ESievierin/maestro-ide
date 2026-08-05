//! Git boundary. All core logic depends on [`GitProvider`], never on the git CLI
//! directly — this is what enables test doubles and a future libgit2/GitLab impl.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

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
}
