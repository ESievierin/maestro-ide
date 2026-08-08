//! GitHub daemon (Этап 3): watches the repository for work addressed to the
//! user and turns it into prepared, human-gated sessions.
//!
//! Two flows, both deliberately read-only:
//! - **issue assigned to the configured account** → research worktree + a
//!   plan-mode session writing `RESEARCH.md`;
//! - **new review comment on a PR whose head branch has a worktree here** → a
//!   read-only session that verifies the comment against the diff and writes
//!   `REVIEW_PLAN.md` with a resolution plan and draft replies. Nothing is
//!   ever posted to GitHub and nothing is committed — acting on the plan is
//!   the human's move, through the ordinary (gated) session flow.
//!
//! Off by default (`daemon_enabled`). One task runs at a time; new work waits
//! in a persistent queue that survives restarts without duplicating anything
//! (task keys are idempotent). A usage gate keeps the background lane from
//! eating the 5-hour window interactive work needs.

pub mod github;

pub use github::{GhAccount, GhCli, GhProvider};

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::session::{SessionManager, SessionType, SpawnParams};
use crate::core::store::{DaemonTask, Store};
use crate::core::worktree::{CreateWorktreeRequest, WorktreeManager};
use crate::error::{MaestroError, Result};

/// `"true"` turns the daemon on. Also toggleable from the daemon panel.
pub const SETTING_DAEMON_ENABLED: &str = "daemon_enabled";
/// The `gh` login the daemon acts as. Empty = gh's currently active account.
pub const SETTING_DAEMON_ACCOUNT: &str = "daemon_account";
/// Poll cadence in minutes (default 5, minimum 1).
pub const SETTING_DAEMON_POLL_MINUTES: &str = "daemon_poll_minutes";
/// Max 5h-window utilization (percent) at which queued tasks may still start.
pub const SETTING_DAEMON_USAGE_THRESHOLD: &str = "daemon_usage_threshold";
/// Model for research sessions (empty = CLI default).
pub const SETTING_DAEMON_RESEARCH_MODEL: &str = "daemon_research_model";
/// Model for PR-comment verification sessions (empty = CLI default).
pub const SETTING_DAEMON_VERIFY_MODEL: &str = "daemon_verify_model";
/// `owner/name` to watch. Empty = derived from the open repository's origin.
pub const SETTING_DAEMON_REPO: &str = "daemon_repo";

const DEFAULT_POLL_MINUTES: u64 = 5;
const DEFAULT_USAGE_THRESHOLD: f64 = 50.0;

/// What the frontend chip/panel shows.
#[derive(Clone, Debug, Serialize)]
pub struct DaemonStatus {
    pub enabled: bool,
    /// The account the daemon is configured to act as (resolved, may be empty).
    pub account: String,
    /// All gh logins on this machine, for the account picker.
    pub accounts: Vec<GhAccount>,
    /// Repository being watched (`owner/name`), once resolved.
    pub repo: Option<String>,
    pub queued: usize,
    pub running: Option<DaemonTask>,
    pub last_poll: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Latest known 5h-window utilization (percent), if the CLI reported one.
    pub utilization: Option<f64>,
}

/// What the manager needs from the rest of the core — a seam for tests.
pub trait DaemonExec: Send + Sync {
    /// The selected repository's root, when one is open.
    fn repo_path(&self) -> Result<Option<PathBuf>>;
    /// `(branch, worktree_path)` for a branch that has a worktree, if any.
    fn worktree_for_branch(&self, branch: &str) -> Result<Option<(String, String)>>;
    /// Get-or-create the research worktree for an issue. `known_branch` is the
    /// branch a previous (requeued) attempt already created.
    fn ensure_research_worktree(
        &self,
        task_id: &str,
        slug: &str,
        known_branch: Option<&str>,
    ) -> Result<(String, String)>;
    /// Spawn a session; returns its id.
    fn spawn(&self, params: SpawnParams) -> Result<String>;
}

/// Production executor over the real managers.
pub struct RealDaemonExec {
    pub worktrees: Arc<WorktreeManager>,
    pub sessions: Arc<SessionManager>,
}

impl DaemonExec for RealDaemonExec {
    fn repo_path(&self) -> Result<Option<PathBuf>> {
        Ok(self.worktrees.repo_info()?.map(|r| r.path))
    }

    fn worktree_for_branch(&self, branch: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .map(|w| (branch.to_string(), w.path.to_string_lossy().into_owned())))
    }

    fn ensure_research_worktree(
        &self,
        task_id: &str,
        slug: &str,
        known_branch: Option<&str>,
    ) -> Result<(String, String)> {
        if let Some(branch) = known_branch {
            if let Some(found) = self.worktree_for_branch(branch)? {
                return Ok(found);
            }
            // Branch survived a worktree removal (or a restart): reattach it.
            let info = self.worktrees.create(CreateWorktreeRequest {
                existing_branch: Some(branch.to_string()),
                kind: None,
                task_id: None,
                slug: None,
                base: None,
            })?;
            return Ok((
                info.branch.unwrap_or_else(|| branch.to_string()),
                info.path.to_string_lossy().into_owned(),
            ));
        }
        let info = self.worktrees.create(CreateWorktreeRequest {
            existing_branch: None,
            kind: Some("research".into()),
            task_id: Some(task_id.to_string()),
            slug: Some(slug.to_string()),
            base: None,
        })?;
        let branch = info.branch.ok_or_else(|| MaestroError::InvalidData {
            message: "created worktree has no branch".into(),
        })?;
        Ok((branch, info.path.to_string_lossy().into_owned()))
    }

    fn spawn(&self, params: SpawnParams) -> Result<String> {
        Ok(self.sessions.spawn(params)?.id)
    }
}

struct DaemonRuntime {
    last_poll: Option<DateTime<Utc>>,
    last_error: Option<String>,
    utilization: Option<f64>,
    /// Cached slug of the watched repository, refreshed each poll.
    repo: Option<String>,
}

pub struct DaemonManager {
    store: Arc<dyn Store>,
    gh: Arc<dyn GhProvider>,
    exec: Arc<dyn DaemonExec>,
    bus: EventBus,
    runtime: Mutex<DaemonRuntime>,
}

impl DaemonManager {
    pub fn new(
        store: Arc<dyn Store>,
        gh: Arc<dyn GhProvider>,
        exec: Arc<dyn DaemonExec>,
        bus: EventBus,
    ) -> Self {
        // Restart safety: tasks that were mid-flight get their turn again; the
        // sessions they had were already failed by fail_stale_sessions.
        if let Ok(store_ref) = store.requeue_running_daemon_tasks() {
            if store_ref > 0 {
                tracing::info!(requeued = store_ref, "daemon tasks requeued after restart");
            }
        }
        Self {
            store,
            gh,
            exec,
            bus,
            runtime: Mutex::new(DaemonRuntime {
                last_poll: None,
                last_error: None,
                utilization: None,
                repo: None,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.setting(SETTING_DAEMON_ENABLED)
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.store.set_setting(
            SETTING_DAEMON_ENABLED,
            if enabled { "true" } else { "false" },
        )?;
        self.bus.publish(Event::DaemonUpdated {});
        Ok(())
    }

    pub fn set_account(&self, account: &str) -> Result<()> {
        self.store.set_setting(SETTING_DAEMON_ACCOUNT, account)?;
        self.bus.publish(Event::DaemonUpdated {});
        Ok(())
    }

    /// The account the daemon acts as: the setting, else gh's active account.
    fn account(&self, accounts: &[GhAccount]) -> String {
        self.setting(SETTING_DAEMON_ACCOUNT)
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| {
                accounts
                    .iter()
                    .find(|a| a.active)
                    .map(|a| a.login.clone())
                    .unwrap_or_default()
            })
    }

    pub fn status(&self) -> DaemonStatus {
        let accounts = self.gh.accounts().unwrap_or_default();
        let account = self.account(&accounts);
        let tasks = self.store.list_daemon_tasks().unwrap_or_default();
        let queued = tasks.iter().filter(|t| t.state == "queued").count();
        let running = tasks.into_iter().find(|t| t.state == "running");
        let runtime = self.runtime.lock().ok();
        DaemonStatus {
            enabled: self.enabled(),
            account,
            accounts,
            repo: runtime.as_ref().and_then(|r| r.repo.clone()),
            queued,
            running,
            last_poll: runtime.as_ref().and_then(|r| r.last_poll),
            last_error: runtime.as_ref().and_then(|r| r.last_error.clone()),
            utilization: runtime.as_ref().and_then(|r| r.utilization),
        }
    }

    pub fn list_tasks(&self) -> Result<Vec<DaemonTask>> {
        self.store.list_daemon_tasks()
    }

    pub fn dismiss_task(&self, key: &str) -> Result<()> {
        self.store
            .update_daemon_task(key, "dismissed", None, None)?;
        self.bus.publish(Event::DaemonUpdated {});
        Ok(())
    }

    /// The main loop: poll on cadence, react to session completions and
    /// rate-limit reports, keep the queue moving.
    pub async fn run_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if !self.enabled() {
                        continue;
                    }
                    if self.poll_due() {
                        self.poll_once();
                    }
                    self.drive_queue();
                }
                event = rx.recv() => match event {
                    Ok(Event::SessionRateLimit { utilization, .. }) => {
                        if let Ok(mut runtime) = self.runtime.lock() {
                            runtime.utilization = utilization;
                        }
                    }
                    Ok(Event::SessionStatusChanged { session_id, status, .. }) => {
                        if status.is_terminal() {
                            self.on_session_finished(&session_id, status.as_str() == "done");
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "daemon loop lagged behind the bus");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }
    }

    fn poll_due(&self) -> bool {
        let minutes = self
            .setting(SETTING_DAEMON_POLL_MINUTES)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_POLL_MINUTES)
            .max(1);
        self.runtime
            .lock()
            .ok()
            .and_then(|r| r.last_poll)
            .map(|last| Utc::now() - last >= chrono::Duration::seconds(minutes as i64 * 60))
            .unwrap_or(true)
    }

    /// One polling pass: resolve account/repo, fetch, enqueue what's new.
    /// Public for tests; errors land in `last_error` (and the status chip),
    /// never panic the loop.
    pub fn poll_once(&self) {
        let result = self.try_poll();
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.last_poll = Some(Utc::now());
            runtime.last_error = result.as_ref().err().map(|e| e.to_string());
        }
        self.bus.publish(Event::DaemonUpdated {});
        if let Err(err) = result {
            tracing::warn!(error = %err, "daemon poll failed");
        }
    }

    fn try_poll(&self) -> Result<()> {
        let accounts = self.gh.accounts()?;
        let account = self.account(&accounts);
        if account.is_empty() {
            return Err(MaestroError::Config {
                message: "no gh account available — log in with `gh auth login` and pick one in the daemon panel".into(),
            });
        }
        let token = self.gh.token(&account)?;
        let slug = self.watched_repo()?;
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.repo = Some(slug.clone());
        }

        let mut new_tasks = 0usize;

        for issue in self.gh.my_issues(&token, &slug, &account)? {
            let key = format!("issue:{slug}#{}", issue.number);
            let payload = serde_json::json!({
                "number": issue.number,
                "title": issue.title,
                "body": issue.body.clone().unwrap_or_default(),
                "url": issue.html_url,
            });
            if self.enqueue(
                &key,
                "issue",
                &format!("#{} {}", issue.number, issue.title),
                &payload,
            )? {
                new_tasks += 1;
            }
        }

        for pull in self.gh.open_pulls(&token, &slug)? {
            // Only PRs whose head branch has a worktree here are ours to watch.
            if self.exec.worktree_for_branch(&pull.head_ref)?.is_none() {
                continue;
            }
            for comment in self.gh.pull_comments(&token, &slug, pull.number)? {
                let key = format!("pr-comment:{slug}#{}:{}", pull.number, comment.id);
                let payload = serde_json::json!({
                    "pr": pull.number,
                    "pr_title": pull.title,
                    "head_ref": pull.head_ref,
                    "comment_id": comment.id,
                    "author": comment.author,
                    "path": comment.path,
                    "body": comment.body,
                    "url": comment.url,
                });
                let title = format!("PR #{} review comment by {}", pull.number, comment.author);
                if self.enqueue(&key, "pr_comment", &title, &payload)? {
                    new_tasks += 1;
                }
            }
        }

        if new_tasks > 0 {
            tracing::info!(new_tasks, repo = %slug, "daemon queued new work");
        }
        Ok(())
    }

    fn enqueue(
        &self,
        key: &str,
        kind: &str,
        title: &str,
        payload: &serde_json::Value,
    ) -> Result<bool> {
        let now = Utc::now();
        self.store.insert_daemon_task(&DaemonTask {
            key: key.to_string(),
            kind: kind.to_string(),
            state: "queued".to_string(),
            title: title.to_string(),
            payload: payload.to_string(),
            branch: None,
            session_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    /// Start the oldest queued task when nothing is running and the usage gate
    /// allows it. Public for tests.
    pub fn drive_queue(&self) {
        if !self.enabled() {
            return;
        }
        match self.store.running_daemon_task() {
            Ok(Some(_)) => return, // serial by design
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(error = %err, "daemon queue read failed");
                return;
            }
        }
        if !self.usage_gate_open() {
            return;
        }
        let task = match self.store.next_queued_daemon_task() {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(error = %err, "daemon queue read failed");
                return;
            }
        };
        if let Err(err) = self.start(&task) {
            let _ = self
                .store
                .update_daemon_task(&task.key, "failed", None, None);
            crate::error::report(&self.bus, &err);
            tracing::warn!(key = %task.key, error = %err, "daemon task failed to start");
        }
        self.bus.publish(Event::DaemonUpdated {});
    }

    fn usage_gate_open(&self) -> bool {
        let threshold = self
            .setting(SETTING_DAEMON_USAGE_THRESHOLD)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(DEFAULT_USAGE_THRESHOLD);
        match self.runtime.lock().ok().and_then(|r| r.utilization) {
            Some(utilization) => utilization <= threshold,
            // No report yet — nothing indicates pressure; let the task through.
            None => true,
        }
    }

    fn start(&self, task: &DaemonTask) -> Result<()> {
        let payload: serde_json::Value =
            serde_json::from_str(&task.payload).map_err(|err| MaestroError::InvalidData {
                message: format!("corrupt daemon task payload: {err}"),
            })?;
        match task.kind.as_str() {
            "issue" => self.start_issue(task, &payload),
            "pr_comment" => self.start_pr_comment(task, &payload),
            other => Err(MaestroError::InvalidData {
                message: format!("unknown daemon task kind: {other}"),
            }),
        }
    }

    fn start_issue(&self, task: &DaemonTask, payload: &serde_json::Value) -> Result<()> {
        let number = payload.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let title = payload.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let body = payload.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let url = payload.get("url").and_then(|u| u.as_str()).unwrap_or("");

        let (branch, cwd) = self.exec.ensure_research_worktree(
            &format!("GH-{number}"),
            title,
            task.branch.as_deref(),
        )?;

        let prompt = format!(
            "You are doing preliminary research for a GitHub issue that was just assigned.\n\n\
             Issue #{number}: {title}\n{url}\n\n{body}\n\n\
             Explore the codebase (read-only) and write your findings to RESEARCH.md in the \
             worktree root: what the issue is really about, which files/modules are involved, \
             a suggested approach, open questions, and risks. Do not modify any other files."
        );
        let session_id = self.exec.spawn(SpawnParams {
            branch: branch.clone(),
            cwd,
            session_type: SessionType::Research,
            model: self
                .setting(SETTING_DAEMON_RESEARCH_MODEL)
                .filter(|m| !m.is_empty()),
            effort: None,
            permission_mode: Some("plan".into()),
            thinking: None,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            prompt,
            resume_from: None,
        })?;

        self.store
            .update_daemon_task(&task.key, "running", Some(&branch), Some(&session_id))?;
        tracing::info!(key = %task.key, branch, session_id, "daemon research started");
        Ok(())
    }

    fn start_pr_comment(&self, task: &DaemonTask, payload: &serde_json::Value) -> Result<()> {
        let head_ref = payload
            .get("head_ref")
            .and_then(|h| h.as_str())
            .unwrap_or("");
        let Some((branch, cwd)) = self.exec.worktree_for_branch(head_ref)? else {
            // The worktree is gone since the comment was queued — nothing to
            // verify against; dismissed, not failed.
            self.store
                .update_daemon_task(&task.key, "dismissed", None, None)?;
            tracing::info!(key = %task.key, head_ref, "daemon task dismissed: no worktree");
            return Ok(());
        };

        let pr = payload.get("pr").and_then(|n| n.as_u64()).unwrap_or(0);
        let author = payload.get("author").and_then(|a| a.as_str()).unwrap_or("");
        let path = payload.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let body = payload.get("body").and_then(|b| b.as_str()).unwrap_or("");
        let url = payload.get("url").and_then(|u| u.as_str()).unwrap_or("");

        let prompt = format!(
            "A review comment arrived on PR #{pr} (branch {branch}).\n\n\
             Author: {author}\nFile: {path}\n{url}\n\nComment:\n{body}\n\n\
             Read the relevant code (read-only) and judge whether the comment is actionable \
             and correct. Write REVIEW_PLAN.md in the worktree root with: your verdict, a \
             concrete resolution plan if action is needed, and a draft reply to the reviewer. \
             Do NOT modify any other files, do NOT commit, and do NOT post anything to GitHub — \
             a human reviews the plan first."
        );
        let session_id = self.exec.spawn(SpawnParams {
            branch: branch.clone(),
            cwd,
            session_type: SessionType::Research,
            model: self
                .setting(SETTING_DAEMON_VERIFY_MODEL)
                .filter(|m| !m.is_empty()),
            effort: None,
            permission_mode: Some("plan".into()),
            thinking: None,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            prompt,
            resume_from: None,
        })?;

        self.store
            .update_daemon_task(&task.key, "running", Some(&branch), Some(&session_id))?;
        tracing::info!(key = %task.key, branch, session_id, "daemon verification started");
        Ok(())
    }

    fn on_session_finished(&self, session_id: &str, ok: bool) {
        let running = match self.store.running_daemon_task() {
            Ok(Some(task)) if task.session_id.as_deref() == Some(session_id) => task,
            _ => return,
        };
        let state = if ok { "done" } else { "failed" };
        if let Err(err) = self
            .store
            .update_daemon_task(&running.key, state, None, None)
        {
            tracing::warn!(error = %err, "daemon task state update failed");
        }
        self.bus.publish(Event::DaemonTaskFinished {
            key: running.key.clone(),
            title: running.title.clone(),
            ok,
        });
        self.bus.publish(Event::DaemonUpdated {});
        tracing::info!(key = %running.key, ok, "daemon task finished");
        // Something else may be waiting.
        self.drive_queue();
    }

    /// The `owner/name` to watch: the setting, else derived from the open
    /// repository's `origin` remote.
    fn watched_repo(&self) -> Result<String> {
        if let Some(repo) = self
            .setting(SETTING_DAEMON_REPO)
            .filter(|r| !r.trim().is_empty())
        {
            return Ok(repo.trim().to_string());
        }
        let path = self.exec.repo_path()?.ok_or_else(|| MaestroError::Config {
            message: "no repository selected — the daemon has nothing to watch".into(),
        })?;
        let url = origin_url(&path)?;
        github::slug_from_remote_url(&url).ok_or_else(|| MaestroError::Config {
            message: format!("could not derive owner/name from remote '{url}' — set `daemon_repo`"),
        })
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.store.get_setting(key).ok().flatten()
    }

    #[cfg(test)]
    fn set_utilization_for_test(&self, utilization: Option<f64>) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.utilization = utilization;
        }
    }
}

/// `git remote get-url origin` in `repo` — no gh, no account dependency.
fn origin_url(repo: &std::path::Path) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().map_err(|err| MaestroError::Config {
        message: format!("failed to launch git: {err}"),
    })?;
    if !output.status.success() {
        return Err(MaestroError::Config {
            message: "the repository has no 'origin' remote — set `daemon_repo` instead".into(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::github::{GhComment, GhIssue, GhPull};
    use super::*;
    use crate::core::store::SqliteStore;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockGh {
        issues: Vec<GhIssue>,
        pulls: Vec<GhPull>,
        comments: Vec<GhComment>,
        token_fails: bool,
    }

    impl GhProvider for MockGh {
        fn accounts(&self) -> Result<Vec<GhAccount>> {
            Ok(vec![
                GhAccount {
                    login: "personal".into(),
                    active: true,
                },
                GhAccount {
                    login: "work".into(),
                    active: false,
                },
            ])
        }
        fn token(&self, account: &str) -> Result<String> {
            if self.token_fails {
                return Err(MaestroError::Config {
                    message: format!("no token for {account}"),
                });
            }
            Ok(format!("tok-{account}"))
        }
        fn my_issues(&self, _t: &str, _s: &str, _l: &str) -> Result<Vec<GhIssue>> {
            Ok(self.issues.clone())
        }
        fn open_pulls(&self, _t: &str, _s: &str) -> Result<Vec<GhPull>> {
            Ok(self.pulls.clone())
        }
        fn pull_comments(&self, _t: &str, _s: &str, _n: u64) -> Result<Vec<GhComment>> {
            Ok(self.comments.clone())
        }
    }

    #[derive(Default)]
    struct MockExec {
        worktrees: Vec<String>,
        created: StdMutex<Vec<(String, String)>>,
        spawned: StdMutex<Vec<SpawnParams>>,
    }

    impl DaemonExec for MockExec {
        fn repo_path(&self) -> Result<Option<PathBuf>> {
            Ok(Some(PathBuf::from("/repo")))
        }
        fn worktree_for_branch(&self, branch: &str) -> Result<Option<(String, String)>> {
            Ok(self
                .worktrees
                .iter()
                .find(|b| b.as_str() == branch)
                .map(|b| (b.clone(), format!("/wt/{b}"))))
        }
        fn ensure_research_worktree(
            &self,
            task_id: &str,
            slug: &str,
            _known: Option<&str>,
        ) -> Result<(String, String)> {
            let branch = format!("research/{task_id}-x");
            self.created
                .lock()
                .unwrap()
                .push((task_id.to_string(), slug.to_string()));
            Ok((branch.clone(), format!("/wt/{branch}")))
        }
        fn spawn(&self, params: SpawnParams) -> Result<String> {
            let id = format!("sess-{}", self.spawned.lock().unwrap().len() + 1);
            self.spawned.lock().unwrap().push(params);
            Ok(id)
        }
    }

    fn manager(gh: MockGh, exec: MockExec) -> (Arc<DaemonManager>, Arc<MockExec>, EventBus) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.set_setting(SETTING_DAEMON_ENABLED, "true").unwrap();
        store
            .set_setting(SETTING_DAEMON_REPO, "owner/repo")
            .unwrap();
        let exec = Arc::new(exec);
        let mgr = Arc::new(DaemonManager::new(
            store,
            Arc::new(gh),
            exec.clone(),
            bus.clone(),
        ));
        (mgr, exec, bus)
    }

    fn issue(number: u64, title: &str) -> GhIssue {
        GhIssue {
            number,
            title: title.into(),
            body: Some("details".into()),
            html_url: format!("https://github.com/owner/repo/issues/{number}"),
        }
    }

    #[test]
    fn polling_is_idempotent_across_ticks() {
        let gh = MockGh {
            issues: vec![issue(7, "Fix retries")],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());

        mgr.poll_once();
        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1, "one issue, one task, however often we poll");
        assert_eq!(tasks[0].state, "queued");
    }

    #[test]
    fn issue_flow_creates_research_worktree_and_plan_session() {
        let gh = MockGh {
            issues: vec![issue(7, "Fix retries")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, MockExec::default());

        mgr.poll_once();
        mgr.drive_queue();

        let created = exec.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "GH-7");

        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].session_type, SessionType::Research);
        assert_eq!(spawned[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawned[0].prompt.contains("Fix retries"));
        assert!(spawned[0].prompt.contains("RESEARCH.md"));

        let running = mgr.store.running_daemon_task().unwrap().expect("running");
        assert_eq!(running.session_id.as_deref(), Some("sess-1"));
        assert_eq!(running.branch.as_deref(), Some("research/GH-7-x"));
    }

    #[test]
    fn pr_comment_flow_routes_by_head_ref_and_dismisses_when_gone() {
        let gh = MockGh {
            pulls: vec![
                GhPull {
                    number: 12,
                    title: "Add retry".into(),
                    head_ref: "impl/T-1-x".into(),
                },
                GhPull {
                    number: 13,
                    title: "Not ours".into(),
                    head_ref: "someone/elses-branch".into(),
                },
            ],
            comments: vec![GhComment {
                id: 501,
                body: "This loop never terminates".into(),
                author: "reviewer".into(),
                path: Some("src/retry.rs".into()),
                url: "https://github.com/owner/repo/pull/12#discussion_r501".into(),
            }],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        // Only PR 12's comment is queued: PR 13's head has no worktree here.
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].key.contains("#12:501"));

        mgr.drive_queue();
        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].branch, "impl/T-1-x");
        assert_eq!(spawned[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawned[0].prompt.contains("REVIEW_PLAN.md"));
        assert!(spawned[0].prompt.contains("do NOT post anything"));
    }

    #[test]
    fn usage_gate_holds_the_queue_until_the_window_clears() {
        let gh = MockGh {
            issues: vec![issue(7, "Fix retries")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, MockExec::default());
        mgr.poll_once();

        mgr.set_utilization_for_test(Some(80.0));
        mgr.drive_queue();
        assert!(
            exec.spawned.lock().unwrap().is_empty(),
            "80% utilization is over the 50% default threshold"
        );
        let task = mgr.store.next_queued_daemon_task().unwrap();
        assert!(task.is_some(), "the task waits, it is not lost");

        mgr.set_utilization_for_test(Some(10.0));
        mgr.drive_queue();
        assert_eq!(exec.spawned.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_token_puts_the_daemon_in_a_visible_error_state() {
        let gh = MockGh {
            token_fails: true,
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.poll_once();
        let status = mgr.status();
        assert!(status.last_error.is_some());
        assert!(status.last_error.unwrap().contains("no token"));
        assert_eq!(mgr.list_tasks().unwrap().len(), 0);
    }

    #[test]
    fn session_completion_finishes_the_task_and_frees_the_lane() {
        let gh = MockGh {
            issues: vec![issue(7, "Fix retries"), issue(8, "Add docs")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, MockExec::default());
        mgr.poll_once();
        mgr.drive_queue();
        assert_eq!(exec.spawned.lock().unwrap().len(), 1);

        mgr.on_session_finished("sess-1", true);
        // Finishing the first task pulls the second one in.
        assert_eq!(exec.spawned.lock().unwrap().len(), 2);
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.iter().filter(|t| t.state == "done").count(),
            1,
            "the finished task is recorded as done"
        );
    }
}
