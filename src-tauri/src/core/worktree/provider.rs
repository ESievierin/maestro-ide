//! Git boundary. All core logic depends on [`GitProvider`], never on the git CLI
//! directly — this is what enables test doubles and a future libgit2/GitLab impl.

use std::path::{Path, PathBuf};

use serde::Serialize;

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
}
