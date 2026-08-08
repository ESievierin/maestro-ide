//! Diff engine (strictly scoped per the brief): unified diff of a branch against the
//! merge-base with its base branch, changed-file list, and on-demand blame.
//!
//! Snapshots are cached per branch; the cache is invalidated when a session on that
//! branch finishes (`session.status_changed` → done) or on manual refresh. Every
//! invalidation publishes `diff.updated` — panels refetch, nothing is pushed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::session::SessionStatus;
use crate::core::store::Store;
use crate::core::worktree::{BlameLine, ChangedFile, GitProvider, WorktreeManager};
use crate::error::{MaestroError, Result};

/// What the diff is computed against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    /// Committed changes only: merge-base → branch head.
    Branch,
    /// Everything in the working tree: merge-base → files on disk, including
    /// uncommitted edits and untracked files. The default: this is what you want
    /// to see while an agent is still working.
    #[default]
    Worktree,
}

/// Computed diff state for one branch.
#[derive(Clone, Debug, Serialize)]
pub struct DiffSnapshot {
    pub branch: String,
    pub scope: DiffScope,
    pub base: String,
    pub merge_base: String,
    pub files: Vec<ChangedFile>,
    /// Raw unified diff.
    pub unified: String,
    pub computed_at: DateTime<Utc>,
}

/// Old/new contents of one changed file, for the unified editor view.
#[derive(Clone, Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    /// Contents at the merge-base; `None` for added files.
    pub old: Option<String>,
    /// Contents at the branch head; `None` for deleted files.
    pub new: Option<String>,
    /// Set when the file was too large to send: the viewer shows this instead of trying to
    /// render it. Generated files and vendored bundles are the usual reason, and CodeMirror
    /// on a multi-megabyte side is what freezes the window.
    pub too_large: Option<String>,
}

/// Largest side of a file diff Maestro will hand to the editor. A real repository has
/// generated files an order of magnitude past this, and a frozen window helps nobody.
pub const MAX_FILE_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// `\r\n` → `\n`. A worktree checkout on Windows with `core.autocrlf` disagrees with
/// a git blob's usually-LF content, and the frontend's own diff of these two plain
/// strings has no CRLF awareness of its own — see `DiffManager::file_diff`.
fn normalize_line_endings(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

pub struct DiffManager {
    git: Arc<dyn GitProvider>,
    store: Arc<dyn Store>,
    worktrees: Arc<WorktreeManager>,
    bus: EventBus,
    cache: Mutex<HashMap<(String, DiffScope), Arc<DiffSnapshot>>>,
}

impl DiffManager {
    pub fn new(
        git: Arc<dyn GitProvider>,
        store: Arc<dyn Store>,
        worktrees: Arc<WorktreeManager>,
        bus: EventBus,
    ) -> Self {
        Self {
            git,
            store,
            worktrees,
            bus,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Cached snapshot for `branch` in `scope`, computing it on first access.
    pub fn get(&self, branch: &str, scope: DiffScope) -> Result<Arc<DiffSnapshot>> {
        let key = (branch.to_string(), scope);
        if let Some(snapshot) = self.lock_cache()?.get(&key).cloned() {
            return Ok(snapshot);
        }
        self.compute_and_cache(branch, scope)
    }

    /// Force recompute and publish `diff.updated`.
    pub fn refresh(&self, branch: &str, scope: DiffScope) -> Result<Arc<DiffSnapshot>> {
        let snapshot = self.compute_and_cache(branch, scope)?;
        self.bus.publish(Event::DiffUpdated {
            branch: branch.to_string(),
        });
        Ok(snapshot)
    }

    /// Drop cached snapshots for both scopes and publish `diff.updated`.
    pub fn invalidate(&self, branch: &str) {
        if let Ok(mut cache) = self.lock_cache() {
            cache.remove(&(branch.to_string(), DiffScope::Branch));
            cache.remove(&(branch.to_string(), DiffScope::Worktree));
        }
        self.bus.publish(Event::DiffUpdated {
            branch: branch.to_string(),
        });
    }

    /// Old/new contents for one file of the branch's diff.
    pub fn file_diff(&self, branch: &str, path: &str, scope: DiffScope) -> Result<FileDiff> {
        let repo = self.require_repo()?;
        let snapshot = self.get(branch, scope)?;
        let entry = snapshot.files.iter().find(|f| f.path == path);
        // Renames read the old side from the original path.
        let old_path = entry.and_then(|f| f.old_path.as_deref()).unwrap_or(path);
        // `git show` returns blobs as committed (typically LF-normalized), but a
        // worktree checkout on Windows with core.autocrlf is CRLF — the editor
        // re-diffs `old`/`new` itself on the frontend, and a line-ending mismatch
        // alone makes every line look changed even when only one word differs.
        // Normalizing both sides here is what git's own diff already effectively
        // does (via its own CRLF-aware comparison), just made explicit for the
        // plain strings we hand to CodeMirror.
        let old = self
            .git
            .show_file(&repo, &snapshot.merge_base, old_path)?
            .map(normalize_line_endings);
        let new = match scope {
            DiffScope::Branch => self
                .git
                .show_file(&repo, branch, path)?
                .map(normalize_line_endings),
            DiffScope::Worktree => {
                let worktree = self.worktree_path(branch)?;
                std::fs::read_to_string(worktree.join(path))
                    .ok()
                    .map(normalize_line_endings)
            }
        };
        // Both sides are read before the size check so a huge file still reports *which*
        // side is huge — the answer to "why can I not see this diff" is the file, not us.
        let biggest = old
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0)
            .max(new.as_ref().map(|s| s.len()).unwrap_or(0));
        if biggest > MAX_FILE_DIFF_BYTES {
            tracing::info!(
                path,
                bytes = biggest,
                limit = MAX_FILE_DIFF_BYTES,
                "file diff too large to render"
            );
            return Ok(FileDiff {
                path: path.to_string(),
                old: None,
                new: None,
                too_large: Some(format!(
                    "{} is {:.1} MB — too large to diff in the editor (limit {:.0} MB). \
                     Use git or an editor for this one.",
                    path,
                    biggest as f64 / (1024.0 * 1024.0),
                    MAX_FILE_DIFF_BYTES as f64 / (1024.0 * 1024.0)
                )),
            });
        }

        Ok(FileDiff {
            path: path.to_string(),
            old,
            new,
            too_large: None,
        })
    }

    /// Blame for a line range of `path` in the branch's worktree (T6 uses this to
    /// build line-question context).
    pub fn blame(&self, branch: &str, path: &str, start: u32, end: u32) -> Result<Vec<BlameLine>> {
        let worktree = self.worktree_path(branch)?;
        self.git.blame_range(&worktree, path, start, end)
    }

    fn worktree_path(&self, branch: &str) -> Result<std::path::PathBuf> {
        self.worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .map(|w| w.path)
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })
    }

    /// Bus subscriber: invalidate a branch's diff whenever a session on it finishes.
    pub async fn run_invalidation_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::SessionStatusChanged {
                    branch,
                    status: SessionStatus::Done,
                    ..
                }) => {
                    tracing::debug!(branch, "session done; invalidating diff cache");
                    self.invalidate(&branch);
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "diff invalidator lagged; clearing whole cache");
                    if let Ok(mut cache) = self.lock_cache() {
                        cache.clear();
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn compute_and_cache(&self, branch: &str, scope: DiffScope) -> Result<Arc<DiffSnapshot>> {
        let repo = self.require_repo()?;
        let stored_base = self.base_for(branch)?;
        // Freshen the base against its remote before diffing — otherwise this
        // compares against whatever the local ref happened to point at whenever it
        // was last fetched, which in practice is often frozen at around
        // worktree-creation time.
        let base = self.git.fresh_base_ref(&repo, &stored_base);
        let merge_base = self.git.merge_base(&repo, &base, branch)?;
        let (files, unified) = match scope {
            DiffScope::Branch => (
                self.git.changed_files(&repo, branch, &base)?,
                self.git.merge_base_diff(&repo, branch, &base)?,
            ),
            DiffScope::Worktree => {
                let worktree = self.worktree_path(branch)?;
                (
                    self.git.worktree_changed_files(&worktree, &merge_base)?,
                    self.git.worktree_diff(&worktree, &merge_base)?,
                )
            }
        };
        let snapshot = Arc::new(DiffSnapshot {
            branch: branch.to_string(),
            scope,
            base,
            merge_base,
            files,
            unified,
            computed_at: Utc::now(),
        });
        self.lock_cache()?
            .insert((branch.to_string(), scope), snapshot.clone());
        tracing::debug!(
            branch,
            ?scope,
            files = snapshot.files.len(),
            "diff computed"
        );
        Ok(snapshot)
    }

    /// The branch's stored base branch, falling back to the repo default.
    fn base_for(&self, branch: &str) -> Result<String> {
        if let Some(stored) = self.store.get_branch(branch)? {
            if let Some(base) = stored.base_branch {
                return Ok(base);
            }
        }
        let repo = self.require_repo()?;
        self.git.default_branch(&repo)
    }

    fn require_repo(&self) -> Result<std::path::PathBuf> {
        self.worktrees
            .repo_info()?
            .map(|info| info.path)
            .ok_or_else(|| MaestroError::Config {
                message: "no repository selected".into(),
            })
    }
}

impl DiffManager {
    #[allow(clippy::type_complexity)]
    fn lock_cache(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<(String, DiffScope), Arc<DiffSnapshot>>>> {
        self.cache.lock().map_err(|_| MaestroError::InvalidData {
            message: "diff cache lock poisoned".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;
    use crate::core::worktree::{BranchStatus, MergeOutcome, WorktreeEntry};
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct MockGit {
        diff_calls: Mutex<u32>,
        last_base: Mutex<Option<String>>,
        shown_paths: Mutex<Vec<String>>,
        /// When set, `show_file` returns this many bytes instead of a short string —
        /// for the oversized-file guard.
        huge_bytes: Mutex<Option<usize>>,
    }

    impl GitProvider for MockGit {
        fn is_git_repo(&self, path: &Path) -> Result<bool> {
            Ok(path == Path::new("/repo"))
        }
        fn default_branch(&self, _repo: &Path) -> Result<String> {
            Ok("main".into())
        }
        fn list_branches(&self, _repo: &Path) -> Result<Vec<String>> {
            Ok(vec!["main".into()])
        }
        fn list_remote_branches(&self, _repo: &Path) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn branch_exists(&self, _repo: &Path, _branch: &str) -> Result<bool> {
            Ok(true)
        }
        fn list_worktrees(&self, _repo: &Path) -> Result<Vec<WorktreeEntry>> {
            Ok(vec![
                WorktreeEntry {
                    path: PathBuf::from("/repo"),
                    head: "abc".into(),
                    branch: Some("main".into()),
                    is_primary: true,
                },
                WorktreeEntry {
                    path: PathBuf::from("/repo.worktrees/impl-T-1-x"),
                    head: "def".into(),
                    branch: Some("impl/T-1-x".into()),
                    is_primary: false,
                },
            ])
        }
        fn create_worktree(
            &self,
            _repo: &Path,
            _path: &Path,
            _branch: &str,
            _base: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        fn remove_worktree(&self, _repo: &Path, _path: &Path, _force: bool) -> Result<()> {
            Ok(())
        }
        fn branch_status(&self, _worktree: &Path) -> Result<BranchStatus> {
            Ok(BranchStatus::default())
        }
        fn merge_base_diff(&self, _repo: &Path, _branch: &str, base: &str) -> Result<String> {
            *self.diff_calls.lock().unwrap() += 1;
            *self.last_base.lock().unwrap() = Some(base.to_string());
            Ok("diff --git a/src/lib.rs b/src/lib.rs\n".into())
        }
        fn merge_base(&self, _repo: &Path, _base: &str, _branch: &str) -> Result<String> {
            Ok("mb00000000000000000000000000000000000000".into())
        }
        fn fresh_base_ref(&self, _repo: &Path, base: &str) -> String {
            base.to_string()
        }
        fn changed_files(
            &self,
            _repo: &Path,
            _branch: &str,
            _base: &str,
        ) -> Result<Vec<ChangedFile>> {
            Ok(vec![
                ChangedFile {
                    path: "src/lib.rs".into(),
                    status: "M".into(),
                    old_path: None,
                },
                ChangedFile {
                    path: "src/renamed.rs".into(),
                    status: "R".into(),
                    old_path: Some("src/original.rs".into()),
                },
            ])
        }
        fn show_file(&self, _repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
            self.shown_paths
                .lock()
                .unwrap()
                .push(format!("{rev}:{path}"));
            if let Some(bytes) = *self.huge_bytes.lock().unwrap() {
                return Ok(Some("x".repeat(bytes)));
            }
            Ok(Some(format!("contents of {path} at {rev}")))
        }
        fn worktree_diff(&self, _worktree: &Path, _merge_base: &str) -> Result<String> {
            *self.diff_calls.lock().unwrap() += 1;
            Ok("wt diff".into())
        }
        fn worktree_changed_files(
            &self,
            _worktree: &Path,
            _merge_base: &str,
        ) -> Result<Vec<ChangedFile>> {
            Ok(vec![ChangedFile {
                path: "untracked.txt".into(),
                status: "A".into(),
                old_path: None,
            }])
        }
        fn merge_branch(
            &self,
            _target_worktree: &Path,
            _source_branch: &str,
        ) -> Result<MergeOutcome> {
            Ok(MergeOutcome {
                merged: true,
                conflicts: Vec::new(),
                message: String::new(),
            })
        }
        fn switch_branch(&self, _worktree: &Path, _branch: &str) -> Result<()> {
            Ok(())
        }
        fn commit_all(&self, _worktree: &Path, _message: &str) -> Result<String> {
            Ok("abc1234 mock".into())
        }
        fn snapshot_push(&self, _worktree: &Path, _label: &str) -> Result<()> {
            Ok(())
        }
        fn snapshot_list(&self, _worktree: &Path) -> Result<Vec<crate::core::worktree::Snapshot>> {
            Ok(Vec::new())
        }
        fn snapshot_restore(&self, _worktree: &Path, _id: &str) -> Result<()> {
            Ok(())
        }
        fn snapshot_drop(&self, _worktree: &Path, _id: &str) -> Result<()> {
            Ok(())
        }
        fn push_branch(&self, _worktree: &Path, _branch: &str) -> Result<String> {
            Ok(String::new())
        }
        fn branch_log(
            &self,
            _repo: &Path,
            _branch: &str,
            _base: &str,
            _limit: usize,
        ) -> Result<Vec<crate::core::worktree::LogEntry>> {
            Ok(Vec::new())
        }
        fn blame_range(
            &self,
            _worktree: &Path,
            _path: &str,
            start: u32,
            end: u32,
        ) -> Result<Vec<BlameLine>> {
            Ok((start..=end)
                .map(|line| BlameLine {
                    sha: "abcd1234".into(),
                    author: "Mock".into(),
                    summary: "mock commit".into(),
                    line,
                    content: format!("line {line}"),
                })
                .collect())
        }
    }

    fn setup() -> (Arc<DiffManager>, Arc<MockGit>, EventBus, Arc<SqliteStore>) {
        let bus = EventBus::new();
        let git = Arc::new(MockGit::default());
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let worktrees = Arc::new(WorktreeManager::new(
            git.clone(),
            store.clone(),
            bus.clone(),
        ));
        worktrees.set_repo(Path::new("/repo")).unwrap();
        let manager = Arc::new(DiffManager::new(
            git.clone(),
            store.clone(),
            worktrees,
            bus.clone(),
        ));
        (manager, git, bus, store)
    }

    #[tokio::test]
    async fn get_is_cached_refresh_recomputes() {
        let (manager, git, bus, _store) = setup();
        let mut rx = bus.subscribe();

        let first = manager.get("impl/T-1-x", DiffScope::Branch).unwrap();
        let second = manager.get("impl/T-1-x", DiffScope::Branch).unwrap();
        assert_eq!(*git.diff_calls.lock().unwrap(), 1, "second get was cached");
        assert_eq!(first.merge_base, second.merge_base);

        manager.refresh("impl/T-1-x", DiffScope::Branch).unwrap();
        assert_eq!(*git.diff_calls.lock().unwrap(), 2);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "diff.updated");
    }

    #[tokio::test]
    async fn invalidate_drops_cache_and_publishes() {
        let (manager, git, bus, _store) = setup();
        manager.get("impl/T-1-x", DiffScope::Branch).unwrap();

        let mut rx = bus.subscribe();
        manager.invalidate("impl/T-1-x");
        assert_eq!(rx.recv().await.unwrap().name(), "diff.updated");

        manager.get("impl/T-1-x", DiffScope::Branch).unwrap();
        assert_eq!(
            *git.diff_calls.lock().unwrap(),
            2,
            "recomputed after invalidate"
        );
    }

    #[tokio::test]
    async fn session_done_invalidates_via_bus() {
        let (manager, _git, bus, _store) = setup();
        manager.get("impl/T-1-x", DiffScope::Branch).unwrap();

        tokio::spawn(manager.clone().run_invalidation_loop(bus.clone()));
        // Give the loop a moment to subscribe.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut rx = bus.subscribe();
        bus.publish(Event::SessionStatusChanged {
            session_id: "s1".into(),
            branch: "impl/T-1-x".into(),
            status: SessionStatus::Done,
        });

        // Expect a diff.updated to follow.
        let deadline = std::time::Duration::from_secs(2);
        let got = tokio::time::timeout(deadline, async {
            loop {
                if let Ok(event) = rx.recv().await {
                    if event.name() == "diff.updated" {
                        return true;
                    }
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(got, "session done must trigger diff.updated");
    }

    #[tokio::test]
    async fn an_oversized_file_is_refused_with_a_reason() {
        // A generated bundle is the usual case: the editor would freeze on it, so the core
        // sends the reason instead of the contents.
        let (diffs, git, _bus, _store) = setup();
        *git.huge_bytes.lock().unwrap() = Some(MAX_FILE_DIFF_BYTES + 1);

        let file = diffs
            .file_diff("impl/T-1-x", "src/lib.rs", DiffScope::Branch)
            .expect("file diff");
        assert!(
            file.old.is_none() && file.new.is_none(),
            "contents withheld"
        );
        let message = file.too_large.expect("a reason");
        assert!(message.contains("src/lib.rs"), "{message}");
        assert!(message.contains("too large"), "{message}");

        // A normal file still comes through with both sides.
        *git.huge_bytes.lock().unwrap() = None;
        diffs.invalidate("impl/T-1-x");
        let file = diffs
            .file_diff("impl/T-1-x", "src/lib.rs", DiffScope::Branch)
            .expect("file diff");
        assert!(file.too_large.is_none());
        assert!(file.old.is_some() && file.new.is_some());
    }

    /// The exact bug report: a Windows worktree checkout has CRLF while the git
    /// blob it's diffed against is LF, so the frontend's own re-diff of these two
    /// plain strings sees every line as changed even when only one word differs.
    #[test]
    fn crlf_checkout_is_not_reported_as_edited() {
        assert_eq!(
            normalize_line_endings("fn main() {\r\n    old();\r\n}\r\n".into()),
            "fn main() {\n    old();\n}\n",
        );
        // Already-LF content passes through unchanged (and cheaply — no realloc).
        assert_eq!(
            normalize_line_endings("fn main() {\n    old();\n}\n".into()),
            "fn main() {\n    old();\n}\n",
        );
        // A bare `\r` (old Mac-style) is left alone — only `\r\n` is a checkout
        // artifact here; rewriting lone `\r` would be a different, unrelated fix.
        assert_eq!(normalize_line_endings("a\rb".into()), "a\rb");
    }

    #[tokio::test]
    async fn file_diff_reads_old_side_of_renames() {
        let (manager, git, _bus, _store) = setup();
        let diff = manager
            .file_diff("impl/T-1-x", "src/renamed.rs", DiffScope::Branch)
            .unwrap();
        assert!(diff.old.is_some() && diff.new.is_some());
        let shown = git.shown_paths.lock().unwrap();
        assert!(
            shown.iter().any(|s| s.ends_with(":src/original.rs")),
            "old side must come from the pre-rename path: {shown:?}"
        );
    }

    #[tokio::test]
    async fn base_falls_back_to_default_branch_and_respects_stored_base() {
        let (manager, git, _bus, store) = setup();

        manager.get("impl/T-1-x", DiffScope::Branch).unwrap();
        assert_eq!(git.last_base.lock().unwrap().as_deref(), Some("main"));

        store
            .upsert_branch("impl/T-2-y", None, Some("develop"))
            .unwrap();
        manager.get("impl/T-2-y", DiffScope::Branch).unwrap();
        assert_eq!(git.last_base.lock().unwrap().as_deref(), Some("develop"));
    }

    #[tokio::test]
    async fn blame_resolves_worktree_path() {
        let (manager, _git, _bus, _store) = setup();
        let lines = manager.blame("impl/T-1-x", "src/lib.rs", 3, 5).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line, 3);
        assert!(manager.blame("no-such-branch", "x", 1, 1).is_err());
    }
}
