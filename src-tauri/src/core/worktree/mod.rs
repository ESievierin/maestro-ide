//! Worktree manager: the core module behind the WorktreeList UI.
//!
//! Owns the "which repository are we orchestrating" state, creates/removes worktrees
//! through the [`GitProvider`] boundary, persists branch rows (branch state survives
//! worktree re-creation), and publishes `worktree.*` events on the bus.

mod git_cli;
mod provider;

pub use git_cli::GitCli;
pub use provider::{BlameLine, BranchStatus, ChangedFile, GitProvider, WorktreeEntry};

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::core::bus::{Event, EventBus};
use crate::core::store::Store;
use crate::error::{GitErrorKind, MaestroError, Result};

/// Settings keys used by the manager.
pub const SETTING_REPO_PATH: &str = "repo_path";
pub const SETTING_BRANCH_TEMPLATE: &str = "branch_naming";

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
    /// `None` when status could not be read (e.g. the directory vanished).
    pub status: Option<BranchStatus>,
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
            infos.push(WorktreeInfo {
                branch: entry.branch,
                path: entry.path,
                is_primary: entry.is_primary,
                task_id: stored.as_ref().and_then(|b| b.task_id.clone()),
                base_branch: stored.as_ref().and_then(|b| b.base_branch.clone()),
                status,
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

        let path = worktree_path(&repo, &branch);
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
        Ok(WorktreeInfo {
            branch: Some(branch),
            path,
            is_primary: false,
            task_id: stored.as_ref().and_then(|b| b.task_id.clone()),
            base_branch: stored.as_ref().and_then(|b| b.base_branch.clone()),
            status,
        })
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

/// Where worktrees live: a sibling directory of the repo,
/// `<parent>/<repo-name>.worktrees/<branch-with-dashes>`.
fn worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let root = repo
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.to_path_buf())
        .join(format!("{repo_name}.worktrees"));
    root.join(branch.replace('/', "-"))
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
    fn worktree_path_is_sibling_of_repo() {
        let path = worktree_path(Path::new("C:/work/myrepo"), "impl/T-1-x");
        assert_eq!(path, Path::new("C:/work/myrepo.worktrees/impl-T-1-x"));
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
}
