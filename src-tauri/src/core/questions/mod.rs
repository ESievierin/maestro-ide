//! Line-level question flow (T6).
//!
//! Selecting lines in the diff viewer builds context (file path, hunk text, blame,
//! branch), renders the `line-question` prompt template, and sends it to the
//! worktree's most recently active session (as a follow-up) or a fresh short-lived
//! session, per the `line_question_target` setting. Completion is announced on the
//! bus as `attention.required` — the core always announces; the UI decides whether
//! the user is still looking.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::diff::{DiffManager, DiffScope};
use crate::core::prompts::PromptManager;
use crate::core::session::{
    SessionManager, SessionStatus, SessionType, SpawnParams, READ_ONLY_MODE,
};
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// Setting key controlling where a line question is sent: `"active_session"`
/// (default) sends a follow-up to the most recently active session, falling back to
/// a fresh one when none is live; `"fresh_session"` always spawns a new session.
pub const SETTING_LINE_QUESTION_TARGET: &str = "line_question_target";
const TARGET_FRESH_SESSION: &str = "fresh_session";
const LINE_QUESTION_TEMPLATE: &str = "line-question";

/// What the UI needs to bind the inline answer block to the session stream.
#[derive(Clone, Debug, Serialize)]
pub struct LineQuestionInfo {
    pub question_id: String,
    pub session_id: String,
    pub branch: String,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub question: String,
}

#[derive(Clone, Debug)]
struct PendingQuestion {
    branch: String,
    path: String,
    line_start: u32,
    line_end: u32,
}

pub struct LineQuestionManager {
    diffs: Arc<DiffManager>,
    sessions: Arc<SessionManager>,
    worktrees: Arc<WorktreeManager>,
    store: Arc<dyn Store>,
    prompts: Arc<PromptManager>,
    bus: EventBus,
    /// One pending question per session id — a session answers one line question at
    /// a time, matching the "keep it simple" scope of this task.
    pending: Mutex<HashMap<String, PendingQuestion>>,
}

impl LineQuestionManager {
    pub fn new(
        diffs: Arc<DiffManager>,
        sessions: Arc<SessionManager>,
        worktrees: Arc<WorktreeManager>,
        store: Arc<dyn Store>,
        prompts: Arc<PromptManager>,
        bus: EventBus,
    ) -> Self {
        Self {
            diffs,
            sessions,
            worktrees,
            store,
            prompts,
            bus,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Build context for `path` lines `start..=end` on `branch`, render the
    /// `line-question` template, and dispatch it to a session per the configured
    /// target.
    pub fn ask(
        &self,
        branch: &str,
        path: &str,
        start: u32,
        end: u32,
        question: &str,
    ) -> Result<LineQuestionInfo> {
        let file_diff = self.diffs.file_diff(branch, path, DiffScope::Worktree)?;
        let hunk = render_hunk(file_diff.new.as_deref().unwrap_or(""), start, end);
        let blame = self
            .diffs
            .blame(branch, path, start, end)
            .map(|lines| render_blame(&lines))
            .unwrap_or_default();

        let mut vars = HashMap::new();
        vars.insert("branch".to_string(), branch.to_string());
        vars.insert("file".to_string(), path.to_string());
        vars.insert("line_start".to_string(), start.to_string());
        vars.insert("line_end".to_string(), end.to_string());
        vars.insert("hunk".to_string(), hunk);
        vars.insert("blame".to_string(), blame);
        vars.insert("question".to_string(), question.to_string());
        let rendered = self.prompts.render(LINE_QUESTION_TEMPLATE, &vars)?;

        let target = self
            .store
            .get_setting(SETTING_LINE_QUESTION_TARGET)?
            .unwrap_or_default();

        let session_id = if target == TARGET_FRESH_SESSION {
            self.spawn_fresh(branch, &rendered)?
        } else {
            match self.active_session(branch)? {
                Some(id) => {
                    self.sessions.send(&id, &rendered)?;
                    id
                }
                None => self.spawn_fresh(branch, &rendered)?,
            }
        };

        let question_id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(
                session_id.clone(),
                PendingQuestion {
                    branch: branch.to_string(),
                    path: path.to_string(),
                    line_start: start,
                    line_end: end,
                },
            );
        }

        Ok(LineQuestionInfo {
            question_id,
            session_id,
            branch: branch.to_string(),
            path: path.to_string(),
            line_start: start,
            line_end: end,
            question: question.to_string(),
        })
    }

    /// Bus subscriber: when a session with a pending question reaches
    /// `awaiting_input` or a terminal status, publish `attention.required` for it.
    /// Run as a background task, mirroring `DiffManager::run_invalidation_loop`.
    pub async fn run_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::SessionStatusChanged {
                    session_id, status, ..
                }) if status == SessionStatus::AwaitingInput || status.is_terminal() => {
                    self.complete(&session_id);
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "line-question loop lagged behind the bus");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn complete(&self, session_id: &str) {
        let pending = match self.pending.lock() {
            Ok(mut pending) => pending.remove(session_id),
            Err(_) => None,
        };
        let Some(question) = pending else { return };
        self.bus.publish(Event::AttentionRequired {
            source: "line_question".to_string(),
            branch: Some(question.branch),
            session_id: Some(session_id.to_string()),
            message: format!(
                "Answer ready for {}:{}-{}",
                question.path, question.line_start, question.line_end
            ),
        });
    }

    /// Most recently updated non-terminal session on `branch`, if any.
    fn active_session(&self, branch: &str) -> Result<Option<String>> {
        let mut sessions = self.sessions.list_for_branch(branch)?;
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions
            .into_iter()
            .find(|s| !s.status.is_terminal())
            .map(|s| s.id))
    }

    /// Spawn a read-only research session with `prompt` as its initial message.
    fn spawn_fresh(&self, branch: &str, prompt: &str) -> Result<String> {
        let worktree = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        let session = self.sessions.spawn(SpawnParams {
            branch: branch.to_string(),
            cwd: worktree.path.to_string_lossy().into_owned(),
            session_type: SessionType::Research,
            model: None,
            effort: None,
            permission_mode: Some(READ_ONLY_MODE.to_string()),
            prompt: prompt.to_string(),
            resume_from: None,
        })?;
        Ok(session.id)
    }
}

/// Render `start..=end` (1-based, inclusive) of `contents` with line numbers.
fn render_hunk(contents: &str, start: u32, end: u32) -> String {
    let lines: Vec<&str> = contents.lines().collect();
    let mut out = String::new();
    for line_no in start..=end.max(start) {
        let text = lines
            .get(line_no.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("");
        out.push_str(&format!("{line_no:>5}: {text}\n"));
    }
    out
}

fn render_blame(lines: &[crate::core::worktree::BlameLine]) -> String {
    lines
        .iter()
        .map(|l| {
            let sha = &l.sha[..l.sha.len().min(8)];
            format!("{sha} {} {}", l.author, l.summary)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::{AgentEngine, SpawnSessionRequest};
    use crate::core::store::SqliteStore;
    use crate::core::worktree::{BlameLine, BranchStatus, ChangedFile, GitProvider, WorktreeEntry};
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;

    struct MockGit;

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
        fn merge_base_diff(&self, _repo: &Path, _branch: &str, _base: &str) -> Result<String> {
            Ok(String::new())
        }
        fn merge_base(&self, _repo: &Path, _base: &str, _branch: &str) -> Result<String> {
            Ok("mb00000000000000000000000000000000000000".into())
        }
        fn changed_files(
            &self,
            _repo: &Path,
            _branch: &str,
            _base: &str,
        ) -> Result<Vec<ChangedFile>> {
            Ok(Vec::new())
        }
        fn show_file(&self, _repo: &Path, _rev: &str, _path: &str) -> Result<Option<String>> {
            Ok(Some("old\n".into()))
        }
        fn worktree_diff(&self, _worktree: &Path, _merge_base: &str) -> Result<String> {
            Ok(String::new())
        }
        fn worktree_changed_files(
            &self,
            _worktree: &Path,
            _merge_base: &str,
        ) -> Result<Vec<ChangedFile>> {
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

    #[derive(Default)]
    struct MockEngine {
        spawns: StdMutex<Vec<SpawnSessionRequest>>,
        sent: StdMutex<Vec<(String, String)>>,
    }

    impl AgentEngine for MockEngine {
        fn spawn_session(&self, req: SpawnSessionRequest) -> Result<()> {
            self.spawns.lock().unwrap().push(req);
            Ok(())
        }
        fn send_prompt(&self, session_id: &str, prompt: &str) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((session_id.to_string(), prompt.to_string()));
            Ok(())
        }
        fn interrupt(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn close_session(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn respond_permission(
            &self,
            _request_id: &str,
            _allow: bool,
            _updated_args: Option<Value>,
            _message: Option<String>,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn spawn_params(branch: &str) -> SpawnParams {
        SpawnParams {
            branch: branch.into(),
            cwd: "/repo.worktrees/impl-T-1-x".into(),
            session_type: SessionType::Manual,
            model: None,
            effort: None,
            permission_mode: None,
            prompt: "hi".into(),
            resume_from: None,
        }
    }

    fn setup() -> (
        Arc<LineQuestionManager>,
        EventBus,
        Arc<SessionManager>,
        Arc<MockEngine>,
        Arc<SqliteStore>,
        tempfile::TempDir,
    ) {
        let bus = EventBus::new();
        let git: Arc<dyn GitProvider> = Arc::new(MockGit);
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let worktrees = Arc::new(WorktreeManager::new(
            git.clone(),
            store.clone(),
            bus.clone(),
        ));
        worktrees.set_repo(Path::new("/repo")).unwrap();
        let diffs = Arc::new(DiffManager::new(
            git.clone(),
            store.clone(),
            worktrees.clone(),
            bus.clone(),
        ));
        let engine = Arc::new(MockEngine::default());
        let sessions = Arc::new(SessionManager::new(
            store.clone(),
            bus.clone(),
            engine.clone(),
        ));
        let prompts_dir = tempfile::tempdir().unwrap();
        let prompts = Arc::new(PromptManager::new(prompts_dir.path()).unwrap());
        let manager = Arc::new(LineQuestionManager::new(
            diffs,
            sessions.clone(),
            worktrees,
            store.clone(),
            prompts,
            bus.clone(),
        ));
        (manager, bus, sessions, engine, store, prompts_dir)
    }

    #[tokio::test]
    async fn active_session_gets_a_follow_up_prompt() {
        let (manager, _bus, sessions, engine, _store, _tmp) = setup();
        let active = sessions.spawn(spawn_params("impl/T-1-x")).unwrap();

        let info = manager
            .ask("impl/T-1-x", "src/lib.rs", 3, 5, "why is this needed?")
            .unwrap();

        assert_eq!(info.session_id, active.id);
        assert_eq!(
            engine.spawns.lock().unwrap().len(),
            1,
            "no new session spawned"
        );
        let sent = engine.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, active.id);
        assert!(sent[0].1.contains("why is this needed?"));
    }

    #[tokio::test]
    async fn falls_back_to_fresh_session_when_none_active() {
        let (manager, _bus, sessions, engine, _store, _tmp) = setup();

        let info = manager
            .ask("impl/T-1-x", "src/lib.rs", 3, 5, "why?")
            .unwrap();

        let spawns = engine.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].session_id, info.session_id);
        assert_eq!(spawns[0].session_type, "research");
        assert_eq!(spawns[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawns[0].prompt.contains("why?"));

        let stored = sessions.list_for_branch("impl/T-1-x").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, info.session_id);
    }

    #[tokio::test]
    async fn fresh_session_target_always_spawns_new() {
        let (manager, _bus, sessions, engine, store, _tmp) = setup();
        store
            .set_setting(SETTING_LINE_QUESTION_TARGET, "fresh_session")
            .unwrap();
        let active = sessions.spawn(spawn_params("impl/T-1-x")).unwrap();

        let info = manager
            .ask("impl/T-1-x", "src/lib.rs", 3, 5, "why?")
            .unwrap();

        assert_ne!(info.session_id, active.id);
        assert_eq!(
            engine.spawns.lock().unwrap().len(),
            2,
            "second (fresh) session spawned"
        );
        assert!(
            engine.sent.lock().unwrap().is_empty(),
            "must not follow up on the active session"
        );
    }

    #[tokio::test]
    async fn attention_required_when_session_becomes_awaiting_input() {
        let (manager, bus, _sessions, _engine, _store, _tmp) = setup();
        tokio::spawn(manager.clone().run_loop(bus.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let info = manager
            .ask("impl/T-1-x", "src/lib.rs", 3, 5, "why?")
            .unwrap();

        let mut rx = bus.subscribe();
        bus.publish(Event::SessionStatusChanged {
            session_id: info.session_id.clone(),
            branch: "impl/T-1-x".into(),
            status: SessionStatus::AwaitingInput,
        });

        let deadline = std::time::Duration::from_secs(2);
        let got = tokio::time::timeout(deadline, async {
            loop {
                if let Ok(Event::AttentionRequired {
                    source, session_id, ..
                }) = rx.recv().await
                {
                    if source == "line_question"
                        && session_id.as_deref() == Some(info.session_id.as_str())
                    {
                        return true;
                    }
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(got, "completion must publish attention.required");

        // Consumed on completion — a later terminal status must not re-publish it.
        let mut rx2 = bus.subscribe();
        bus.publish(Event::SessionStatusChanged {
            session_id: info.session_id,
            branch: "impl/T-1-x".into(),
            status: SessionStatus::Done,
        });
        let got_again = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                if let Ok(Event::AttentionRequired { .. }) = rx2.recv().await {
                    return true;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(!got_again, "pending question is cleared after completion");
    }

    #[test]
    fn render_hunk_numbers_the_requested_lines() {
        let out = render_hunk("a\nb\nc\n", 2, 3);
        assert_eq!(out, "    2: b\n    3: c\n");
    }
}
