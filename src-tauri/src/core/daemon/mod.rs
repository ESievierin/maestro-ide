//! Background daemon (Этап 3): watches GitHub and Jira for work addressed to
//! the user and turns it into prepared, human-gated sessions.
//!
//! Three flows, all deliberately read-only:
//! - **review requested on a PR** (the configured account is in
//!   `requested_reviewers`) → fetch the PR branch, get a worktree on it, and
//!   run a plan-mode session that writes `REVIEW.md`: findings per file,
//!   questions, and draft review comments. Re-triggers when the PR head moves.
//! - **new review comment on a PR whose head branch has a worktree here**
//!   (someone reviewing *your* work) → a read-only session that verifies the
//!   comment against the diff and writes `REVIEW_PLAN.md` with a resolution
//!   plan and draft replies.
//! - **Jira issue assigned to you** (only when `jira_base_url`/`jira_email`/
//!   `jira_token` are configured) → research worktree + a plan-mode session
//!   writing `RESEARCH.md`.
//!
//! Nothing is ever posted to GitHub/Jira and nothing is committed — acting on
//! the prepared output is the human's move, through the ordinary (gated)
//! session flow.
//!
//! Off by default (`daemon_enabled`). One task runs at a time; new work waits
//! in a persistent queue that survives restarts without duplicating anything
//! (task keys are idempotent). A usage gate keeps the background lane from
//! eating the 5-hour window interactive work needs.

pub mod github;
pub mod jira;

pub use github::{GhAccount, GhCli, GhProvider};
pub use jira::{JiraCli, JiraConfig, JiraProvider};

use jira::{
    DEFAULT_JQL, SETTING_JIRA_BASE_URL, SETTING_JIRA_EMAIL, SETTING_JIRA_JQL, SETTING_JIRA_TOKEN,
};

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::session::{SessionManager, SessionStatus, SessionType, SpawnParams};
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
/// Model for research sessions (Jira). Empty = [`DEFAULT_RESEARCH_MODEL`].
pub const SETTING_DAEMON_RESEARCH_MODEL: &str = "daemon_research_model";
/// Reasoning effort for research sessions. Empty = [`DEFAULT_RESEARCH_EFFORT`].
pub const SETTING_DAEMON_RESEARCH_EFFORT: &str = "daemon_research_effort";
/// Model for PR-review / PR-comment sessions. Empty = [`DEFAULT_VERIFY_MODEL`].
pub const SETTING_DAEMON_VERIFY_MODEL: &str = "daemon_verify_model";
/// Reasoning effort for PR-review / PR-comment sessions. Empty = [`DEFAULT_VERIFY_EFFORT`].
pub const SETTING_DAEMON_VERIFY_EFFORT: &str = "daemon_verify_effort";
/// `owner/name` to watch. Empty = derived from the open repository's origin.
pub const SETTING_DAEMON_REPO: &str = "daemon_repo";

const DEFAULT_POLL_MINUTES: u64 = 5;
const DEFAULT_USAGE_THRESHOLD: f64 = 50.0;
/// Research is a first pass over unfamiliar ground — a capable model is enough.
const DEFAULT_RESEARCH_MODEL: &str = "sonnet";
const DEFAULT_RESEARCH_EFFORT: &str = "high";
/// Verify produces text a human reviewer will actually read (a PR review, a
/// reply to a comment) — worth the extra reasoning.
const DEFAULT_VERIFY_MODEL: &str = "sonnet";
const DEFAULT_VERIFY_EFFORT: &str = "xhigh";

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
    /// Whether the Jira flow has credentials configured.
    pub jira_configured: bool,
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
    /// Get-or-create a worktree on a PR's head branch for reviewing it —
    /// fetching the branch from origin when it does not exist locally.
    fn ensure_review_worktree(&self, head_ref: &str) -> Result<(String, String)>;
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

    fn ensure_review_worktree(&self, head_ref: &str) -> Result<(String, String)> {
        if let Some(found) = self.worktree_for_branch(head_ref)? {
            // Already tracking this PR locally; refresh best-effort so the
            // review sees the latest push (a stale copy still beats a failure).
            if let Err(err) = self.worktrees.fetch_branch(head_ref) {
                tracing::debug!(head_ref, error = %err, "review branch refresh failed");
            }
            return Ok(found);
        }
        self.worktrees.fetch_branch(head_ref)?;
        let info = self.worktrees.create(CreateWorktreeRequest {
            existing_branch: Some(head_ref.to_string()),
            kind: None,
            task_id: None,
            slug: None,
            base: None,
        })?;
        Ok((
            info.branch.unwrap_or_else(|| head_ref.to_string()),
            info.path.to_string_lossy().into_owned(),
        ))
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
    jira: Arc<dyn JiraProvider>,
    exec: Arc<dyn DaemonExec>,
    bus: EventBus,
    runtime: Mutex<DaemonRuntime>,
}

impl DaemonManager {
    pub fn new(
        store: Arc<dyn Store>,
        gh: Arc<dyn GhProvider>,
        jira: Arc<dyn JiraProvider>,
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
            jira,
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
        resolve_account(self.store.as_ref(), accounts)
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
            jira_configured: self.jira_config().is_some(),
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
                        // A daemon session's own work is done as soon as its first turn
                        // ends — awaiting_input, not just a terminal state. These sessions
                        // are conversational (plan mode, ready for a follow-up); waiting
                        // for the user to close them would stall the whole queue behind
                        // whatever they happen to still have open.
                        if status.is_terminal() || status == SessionStatus::AwaitingInput {
                            let ok = !matches!(status, SessionStatus::Failed | SessionStatus::Cancelled);
                            self.on_session_finished(&session_id, ok);
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

    /// One polling pass: GitHub (review requests + comments on own PRs) and
    /// Jira (when configured) are independent — one failing must not silence
    /// the other. Public for tests; errors land in `last_error` (and the
    /// status chip), never panic the loop.
    pub fn poll_once(&self) {
        let gh_result = self.poll_github();
        let jira_result = self.poll_jira();
        let error = match (gh_result, jira_result) {
            (Ok(()), Ok(())) => None,
            (Err(gh), Ok(())) => Some(gh.to_string()),
            (Ok(()), Err(jira)) => Some(format!("Jira: {jira}")),
            (Err(gh), Err(jira)) => Some(format!("{gh}; Jira: {jira}")),
        };
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.last_poll = Some(Utc::now());
            runtime.last_error = error.clone();
        }
        self.bus.publish(Event::DaemonUpdated {});
        if let Some(err) = error {
            tracing::warn!(error = %err, "daemon poll failed");
        }
    }

    fn poll_github(&self) -> Result<()> {
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

        for pull in self.gh.open_pulls(&token, &slug)? {
            // Someone wants this account's review → prepare one, re-keyed per
            // head SHA so a new push produces a fresh review pass.
            if pull.requested_reviewers.iter().any(|r| r == &account) {
                let key = format!("pr-review:{slug}#{}:{}", pull.number, pull.head_sha);
                let payload = serde_json::json!({
                    "pr": pull.number,
                    "pr_title": pull.title,
                    "pr_body": pull.body,
                    "author": pull.author,
                    "head_ref": pull.head_ref,
                    "head_sha": pull.head_sha,
                    "url": pull.url,
                });
                let title = format!("PR #{} review requested: {}", pull.number, pull.title);
                if self.enqueue(&key, "pr_review", &title, &payload)? {
                    new_tasks += 1;
                }
            }

            // Comments only matter on PRs whose head branch we work on here.
            if self.exec.worktree_for_branch(&pull.head_ref)?.is_none() {
                continue;
            }
            let resolved = self
                .gh
                .resolved_comment_ids(&token, &slug, pull.number)
                .unwrap_or_else(|err| {
                    tracing::debug!(error = %err, pr = pull.number, "could not check resolved threads; treating all comments as unresolved");
                    Default::default()
                });
            // One task per review, not per comment: a reviewer submitting three
            // comments at once should get one plan covering all three, not three
            // sessions each blind to the other two.
            let mut by_review: Vec<(Option<u64>, Vec<github::GhComment>)> = Vec::new();
            for comment in self.gh.pull_comments(&token, &slug, pull.number)? {
                if resolved.contains(&comment.id) {
                    continue;
                }
                match by_review
                    .iter_mut()
                    .find(|(rid, _)| *rid == comment.review_id)
                {
                    Some((_, list)) => list.push(comment),
                    None => by_review.push((comment.review_id, vec![comment])),
                }
            }
            for (review_id, mut comments) in by_review {
                comments.sort_by_key(|c| c.id);
                let key = match review_id {
                    Some(rid) => format!("pr-comment:{slug}#{}:{rid}", pull.number),
                    None => format!(
                        "pr-comment:{slug}#{}:c{}",
                        pull.number,
                        comments
                            .iter()
                            .map(|c| c.id.to_string())
                            .collect::<Vec<_>>()
                            .join("-")
                    ),
                };
                let payload = serde_json::json!({
                    "pr": pull.number,
                    "pr_title": pull.title,
                    "head_ref": pull.head_ref,
                    "comments": comments.iter().map(|c| serde_json::json!({
                        "comment_id": c.id,
                        "author": c.author,
                        "path": c.path,
                        "body": c.body,
                        "url": c.url,
                    })).collect::<Vec<_>>(),
                });
                let authors = comments
                    .iter()
                    .map(|c| c.author.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                let title = format!(
                    "PR #{} — {} review comment{} ({authors})",
                    pull.number,
                    comments.len(),
                    if comments.len() == 1 { "" } else { "s" },
                );
                if self.enqueue(&key, "pr_comment", &title, &payload)? {
                    new_tasks += 1;
                }
            }
        }

        if new_tasks > 0 {
            tracing::info!(new_tasks, repo = %slug, "daemon queued new GitHub work");
        }
        Ok(())
    }

    /// The Jira credentials, when all three are configured.
    fn jira_config(&self) -> Option<JiraConfig> {
        let base_url = self.setting(SETTING_JIRA_BASE_URL)?;
        let email = self.setting(SETTING_JIRA_EMAIL)?;
        let token = self.setting(SETTING_JIRA_TOKEN)?;
        if base_url.trim().is_empty() || email.trim().is_empty() || token.trim().is_empty() {
            return None;
        }
        Some(JiraConfig {
            base_url,
            email,
            token,
            jql: self
                .setting(SETTING_JIRA_JQL)
                .filter(|j| !j.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_JQL.to_string()),
        })
    }

    fn poll_jira(&self) -> Result<()> {
        let Some(config) = self.jira_config() else {
            return Ok(()); // not configured — the flow simply does not exist
        };
        let mut new_tasks = 0usize;
        for issue in self.jira.my_issues(&config)? {
            let key = format!("jira:{}", issue.key);
            let payload = serde_json::json!({
                "key": issue.key,
                "summary": issue.summary,
                "description": issue.description,
                "url": issue.url,
            });
            let title = format!("{} {}", issue.key, issue.summary);
            if self.enqueue(&key, "jira", &title, &payload)? {
                new_tasks += 1;
            }
        }
        if new_tasks > 0 {
            tracing::info!(new_tasks, "daemon queued new Jira work");
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
            "pr_review" => self.start_pr_review(task, &payload),
            "pr_comment" => self.start_pr_comment(task, &payload),
            "jira" => self.start_jira(task, &payload),
            // "issue" is the retired GitHub-issue flow; anything queued by an
            // older build is quietly retired with it.
            other => {
                self.store
                    .update_daemon_task(&task.key, "dismissed", None, None)?;
                tracing::info!(key = %task.key, kind = other, "legacy daemon task dismissed");
                Ok(())
            }
        }
    }

    fn start_pr_review(&self, task: &DaemonTask, payload: &serde_json::Value) -> Result<()> {
        let pr = payload.get("pr").and_then(|n| n.as_u64()).unwrap_or(0);
        let pr_title = payload
            .get("pr_title")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let pr_body = payload
            .get("pr_body")
            .and_then(|b| b.as_str())
            .unwrap_or("");
        let author = payload.get("author").and_then(|a| a.as_str()).unwrap_or("");
        let head_ref = payload
            .get("head_ref")
            .and_then(|h| h.as_str())
            .unwrap_or("");
        let url = payload.get("url").and_then(|u| u.as_str()).unwrap_or("");

        let (branch, cwd) = self.exec.ensure_review_worktree(head_ref)?;

        let prompt = format!(
            "Your review was requested on PR #{pr} by {author}.\n\n\
             Title: {pr_title}\n{url}\n\nPR description:\n{pr_body}\n\n\
             This worktree has the PR branch checked out. Review the changes this branch \
             introduces relative to its base (use read-only git commands like \
             `git log`/`git diff` against the base branch). Write REVIEW.md in the worktree \
             root with: a one-paragraph summary of what the PR does, findings ordered by \
             severity (each with file:line and a concrete explanation), questions for the \
             author, and a draft review comment per finding, ready to paste. \
             Do NOT modify any files, do NOT commit, and do NOT post anything to GitHub — \
             the human decides what feedback to send."
        );
        let (model, effort) = self.verify_model_effort();
        let session_id = self.exec.spawn(SpawnParams {
            branch: branch.clone(),
            cwd,
            session_type: SessionType::Research,
            model,
            effort,
            permission_mode: Some("plan".into()),
            thinking: None,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            prompt,
            resume_from: None,
        })?;

        self.store
            .update_daemon_task(&task.key, "running", Some(&branch), Some(&session_id))?;
        tracing::info!(key = %task.key, branch, session_id, "daemon PR review started");
        Ok(())
    }

    fn start_jira(&self, task: &DaemonTask, payload: &serde_json::Value) -> Result<()> {
        let issue_key = payload.get("key").and_then(|k| k.as_str()).unwrap_or("");
        let summary = payload
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let description = payload
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let url = payload.get("url").and_then(|u| u.as_str()).unwrap_or("");

        let (branch, cwd) =
            self.exec
                .ensure_research_worktree(issue_key, summary, task.branch.as_deref())?;

        let prompt = format!(
            "You are doing preliminary research for a Jira issue that was just assigned.\n\n\
             {issue_key}: {summary}\n{url}\n\n{description}\n\n\
             Explore the codebase (read-only) and write your findings to RESEARCH.md in the \
             worktree root: what the issue is really about, which files/modules are involved, \
             a suggested approach, open questions, and risks. Do not modify any other files."
        );
        let (model, effort) = self.research_model_effort();
        let session_id = self.exec.spawn(SpawnParams {
            branch: branch.clone(),
            cwd,
            session_type: SessionType::Research,
            model,
            effort,
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
        let comments = payload
            .get("comments")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        // Group by file for the same reason PrRepliesDialog's own prompt does:
        // a reviewer's comments almost always cluster by file, and reading them
        // together is how a human would triage a review too.
        let mut by_path: Vec<(String, Vec<&serde_json::Value>)> = Vec::new();
        for comment in &comments {
            let path = comment
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("(general)")
                .to_string();
            match by_path.iter_mut().find(|(p, _)| p == &path) {
                Some((_, list)) => list.push(comment),
                None => by_path.push((path, vec![comment])),
            }
        }
        let mut prompt = format!(
            "{} review comment(s) arrived on PR #{pr}.\n\n",
            comments.len()
        );
        for (path, list) in &by_path {
            prompt.push_str(&format!("## {path}\n"));
            for c in list {
                let id = c.get("comment_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let author = c
                    .get("author")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
                prompt.push_str(&format!("[comment {id}] {author}:\n{body}\n{url}\n\n"));
            }
        }
        prompt.push_str(
            "Read the relevant code (read-only) and judge whether each comment is actionable \
             and correct. Write REVIEW_PLAN.md in the worktree root with: your verdict per \
             comment, a concrete resolution plan for anything needing action, and a draft \
             reply to the reviewer for each. Do NOT modify any other files, do NOT commit, \
             and do NOT post anything to GitHub — a human reviews the plan first.",
        );
        let (model, effort) = self.verify_model_effort();
        let session_id = self.exec.spawn(SpawnParams {
            branch: branch.clone(),
            cwd,
            // review_fix, not research: this is the exact session type the manual
            // reply dialog looks for on the branch, so its plan is already there
            // (and resumable) instead of being invisible to it.
            session_type: SessionType::ReviewFix,
            model,
            effort,
            permission_mode: Some("plan".into()),
            thinking: None,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            prompt,
            resume_from: None,
        })?;

        self.store
            .update_daemon_task(&task.key, "running", Some(&branch), Some(&session_id))?;
        tracing::info!(key = %task.key, branch, session_id, comments = comments.len(), "daemon verification started");
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
        // A PR-comment plan is ready for a human to look at — put it in the
        // attention queue, not just the daemon panel, so it surfaces the same
        // way a permission prompt or gate does.
        if ok && running.kind == "pr_comment" {
            if let Some(branch) = running.branch.clone() {
                self.bus.publish(Event::AttentionRequired {
                    source: "pr_review_ready".to_string(),
                    branch: Some(branch),
                    session_id: running.session_id.clone(),
                    message: format!("Reply plan ready: {}", running.title),
                });
            }
        }
        self.bus.publish(Event::DaemonUpdated {});
        tracing::info!(key = %running.key, ok, "daemon task finished");
        // Something else may be waiting.
        self.drive_queue();
    }

    /// The `owner/name` to watch: the setting, else derived from the open
    /// repository's `origin` remote.
    fn watched_repo(&self) -> Result<String> {
        resolve_slug(self.store.as_ref(), self.exec.repo_path()?.as_deref())
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.store.get_setting(key).ok().flatten()
    }

    /// Model + effort for the "research" bucket (Jira today): a capable model
    /// is enough for a first pass over unfamiliar ground.
    fn research_model_effort(&self) -> (Option<String>, Option<String>) {
        (
            Some(
                self.setting(SETTING_DAEMON_RESEARCH_MODEL)
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| DEFAULT_RESEARCH_MODEL.to_string()),
            ),
            Some(
                self.setting(SETTING_DAEMON_RESEARCH_EFFORT)
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| DEFAULT_RESEARCH_EFFORT.to_string()),
            ),
        )
    }

    /// Model + effort for the "verify" bucket (PR review, PR-comment reply
    /// prep): the text a human reviewer actually reads deserves the extra
    /// reasoning.
    fn verify_model_effort(&self) -> (Option<String>, Option<String>) {
        (
            Some(
                self.setting(SETTING_DAEMON_VERIFY_MODEL)
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| DEFAULT_VERIFY_MODEL.to_string()),
            ),
            Some(
                self.setting(SETTING_DAEMON_VERIFY_EFFORT)
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| DEFAULT_VERIFY_EFFORT.to_string()),
            ),
        )
    }

    #[cfg(test)]
    fn set_utilization_for_test(&self, utilization: Option<f64>) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.utilization = utilization;
        }
    }
}

/// The gh login the app acts as: the `daemon_account` setting when set, else
/// gh's globally active account. Shared by the daemon and the PR actions so
/// "who am I on GitHub" has exactly one answer.
pub fn resolve_account(store: &dyn Store, accounts: &[GhAccount]) -> String {
    store
        .get_setting(SETTING_DAEMON_ACCOUNT)
        .ok()
        .flatten()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| {
            accounts
                .iter()
                .find(|a| a.active)
                .map(|a| a.login.clone())
                .unwrap_or_default()
        })
}

/// The `owner/name` this app talks to: the `daemon_repo` setting when set,
/// else derived from the open repository's `origin` remote.
pub fn resolve_slug(store: &dyn Store, repo_path: Option<&std::path::Path>) -> Result<String> {
    if let Some(repo) = store
        .get_setting(SETTING_DAEMON_REPO)
        .ok()
        .flatten()
        .filter(|r| !r.trim().is_empty())
    {
        return Ok(repo.trim().to_string());
    }
    let path = repo_path.ok_or_else(|| MaestroError::Config {
        message: "no repository selected".into(),
    })?;
    let url = origin_url(path)?;
    github::slug_from_remote_url(&url).ok_or_else(|| MaestroError::Config {
        message: format!("could not derive owner/name from remote '{url}' — set `daemon_repo`"),
    })
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
    use super::github::{GhComment, GhPull};
    use super::jira::JiraIssue;
    use super::*;
    use crate::core::store::SqliteStore;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MockGh {
        pulls: Vec<GhPull>,
        comments: Vec<GhComment>,
        resolved: Vec<u64>,
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
        fn open_pulls(&self, _t: &str, _s: &str) -> Result<Vec<GhPull>> {
            Ok(self.pulls.clone())
        }
        fn pull_comments(&self, _t: &str, _s: &str, _n: u64) -> Result<Vec<GhComment>> {
            Ok(self.comments.clone())
        }
        fn resolved_comment_ids(
            &self,
            _t: &str,
            _s: &str,
            _n: u64,
        ) -> Result<std::collections::HashSet<u64>> {
            Ok(self.resolved.iter().copied().collect())
        }
    }

    #[derive(Default)]
    struct MockJira {
        issues: Vec<JiraIssue>,
        fails: bool,
    }

    impl JiraProvider for MockJira {
        fn my_issues(&self, _config: &JiraConfig) -> Result<Vec<JiraIssue>> {
            if self.fails {
                return Err(MaestroError::Config {
                    message: "401 from Jira".into(),
                });
            }
            Ok(self.issues.clone())
        }
    }

    #[derive(Default)]
    struct MockExec {
        worktrees: Vec<String>,
        created: StdMutex<Vec<(String, String)>>,
        fetched: StdMutex<Vec<String>>,
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
        fn ensure_review_worktree(&self, head_ref: &str) -> Result<(String, String)> {
            self.fetched.lock().unwrap().push(head_ref.to_string());
            Ok((head_ref.to_string(), format!("/wt/{head_ref}")))
        }
        fn spawn(&self, params: SpawnParams) -> Result<String> {
            let id = format!("sess-{}", self.spawned.lock().unwrap().len() + 1);
            self.spawned.lock().unwrap().push(params);
            Ok(id)
        }
    }

    fn manager_full(
        gh: MockGh,
        jira: MockJira,
        exec: MockExec,
        with_jira_config: bool,
    ) -> (Arc<DaemonManager>, Arc<MockExec>, EventBus) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.set_setting(SETTING_DAEMON_ENABLED, "true").unwrap();
        store
            .set_setting(SETTING_DAEMON_REPO, "owner/repo")
            .unwrap();
        if with_jira_config {
            store
                .set_setting(SETTING_JIRA_BASE_URL, "https://org.atlassian.net")
                .unwrap();
            store.set_setting(SETTING_JIRA_EMAIL, "me@org.com").unwrap();
            store.set_setting(SETTING_JIRA_TOKEN, "tok").unwrap();
        }
        let exec = Arc::new(exec);
        let mgr = Arc::new(DaemonManager::new(
            store,
            Arc::new(gh),
            Arc::new(jira),
            exec.clone(),
            bus.clone(),
        ));
        (mgr, exec, bus)
    }

    fn manager(gh: MockGh, exec: MockExec) -> (Arc<DaemonManager>, Arc<MockExec>, EventBus) {
        manager_full(gh, MockJira::default(), exec, false)
    }

    fn pull(number: u64, title: &str, head_ref: &str) -> GhPull {
        GhPull {
            number,
            title: title.into(),
            body: "PR body".into(),
            author: "colleague".into(),
            head_ref: head_ref.into(),
            head_sha: format!("sha-{number}"),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            requested_reviewers: Vec::new(),
        }
    }

    fn review_request(number: u64, title: &str, head_ref: &str) -> GhPull {
        GhPull {
            requested_reviewers: vec!["personal".into()],
            ..pull(number, title, head_ref)
        }
    }

    fn comment(id: u64, review_id: Option<u64>, pr: u64, path: &str) -> GhComment {
        GhComment {
            id,
            body: format!("Comment {id}"),
            author: "reviewer".into(),
            path: Some(path.into()),
            url: format!("https://github.com/owner/repo/pull/{pr}#discussion_r{id}"),
            review_id,
        }
    }

    fn jira_issue(key: &str, summary: &str) -> JiraIssue {
        JiraIssue {
            key: key.into(),
            summary: summary.into(),
            description: "details".into(),
            url: format!("https://org.atlassian.net/browse/{key}"),
        }
    }

    #[test]
    fn polling_is_idempotent_across_ticks() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());

        mgr.poll_once();
        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "one review request, one task, however often we poll"
        );
        assert_eq!(tasks[0].state, "queued");
    }

    #[test]
    fn review_request_fetches_the_pr_branch_and_spawns_a_review_session() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, MockExec::default());

        mgr.poll_once();
        mgr.drive_queue();

        assert_eq!(
            exec.fetched.lock().unwrap().as_slice(),
            ["feature/retry"],
            "the PR head branch is materialized locally"
        );
        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].session_type, SessionType::Research);
        assert_eq!(spawned[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawned[0].prompt.contains("REVIEW.md"));
        assert!(spawned[0].prompt.contains("Add retry"));
        assert!(spawned[0].prompt.contains("do NOT post anything"));
        assert_eq!(
            spawned[0].model.as_deref(),
            Some("sonnet"),
            "the verify bucket defaults to sonnet"
        );
        assert_eq!(
            spawned[0].effort.as_deref(),
            Some("xhigh"),
            "PR review is verify-bucket work — a human will read it"
        );

        let running = mgr.store.running_daemon_task().unwrap().expect("running");
        assert_eq!(running.branch.as_deref(), Some("feature/retry"));
    }

    #[test]
    fn an_explicit_daemon_model_setting_overrides_the_bucket_default() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, MockExec::default());
        mgr.store
            .set_setting(SETTING_DAEMON_VERIFY_MODEL, "opus")
            .unwrap();
        mgr.store
            .set_setting(SETTING_DAEMON_VERIFY_EFFORT, "medium")
            .unwrap();

        mgr.poll_once();
        mgr.drive_queue();

        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned[0].model.as_deref(), Some("opus"));
        assert_eq!(spawned[0].effort.as_deref(), Some("medium"));
    }

    #[test]
    fn a_new_push_to_a_reviewed_pr_queues_a_fresh_review() {
        let mut first = review_request(12, "Add retry", "feature/retry");
        first.head_sha = "sha-old".into();
        let gh = MockGh {
            pulls: vec![first],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.poll_once();
        assert_eq!(mgr.list_tasks().unwrap().len(), 1);

        // Same PR, new head SHA → a second, distinct task.
        let mut second = review_request(12, "Add retry", "feature/retry");
        second.head_sha = "sha-new".into();
        let gh = MockGh {
            pulls: vec![second],
            ..Default::default()
        };
        let store = mgr.store.clone();
        let bus = EventBus::new();
        let mgr2 = Arc::new(DaemonManager::new(
            store,
            Arc::new(gh),
            Arc::new(MockJira::default()),
            Arc::new(MockExec::default()),
            bus,
        ));
        mgr2.poll_once();
        assert_eq!(mgr2.list_tasks().unwrap().len(), 2);
    }

    #[test]
    fn pr_comment_flow_routes_by_head_ref_and_skips_foreign_prs() {
        let gh = MockGh {
            pulls: vec![
                pull(12, "Add retry", "impl/T-1-x"),
                pull(13, "Not ours", "someone/elses-branch"),
            ],
            comments: vec![comment(501, Some(900), 12, "src/retry.rs")],
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
        assert!(tasks[0].key.contains("#12:900"));

        mgr.drive_queue();
        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].branch, "impl/T-1-x");
        assert_eq!(spawned[0].session_type, SessionType::ReviewFix);
        assert_eq!(spawned[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawned[0].prompt.contains("REVIEW_PLAN.md"));
        assert!(spawned[0].prompt.contains("do NOT post anything"));
    }

    #[test]
    fn comments_from_the_same_review_become_one_task_not_several() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![
                comment(501, Some(900), 12, "src/retry.rs"),
                comment(502, Some(900), 12, "src/retry.rs"),
                comment(503, Some(900), 12, "src/lib.rs"),
            ],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "one review, one task, regardless of comment count"
        );
        assert!(tasks[0].key.contains("#12:900"));

        mgr.drive_queue();
        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        // All three comments made it into the one prompt.
        assert!(spawned[0].prompt.contains("[comment 501]"));
        assert!(spawned[0].prompt.contains("[comment 502]"));
        assert!(spawned[0].prompt.contains("[comment 503]"));
        assert!(spawned[0].prompt.contains("## src/retry.rs"));
        assert!(spawned[0].prompt.contains("## src/lib.rs"));
    }

    #[test]
    fn comments_from_different_reviews_become_separate_tasks() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![
                comment(501, Some(900), 12, "src/retry.rs"),
                comment(601, Some(901), 12, "src/retry.rs"),
            ],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.key.contains("#12:900")));
        assert!(tasks.iter().any(|t| t.key.contains("#12:901")));
    }

    #[test]
    fn a_resolved_comment_thread_is_never_queued() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![
                comment(501, Some(900), 12, "src/retry.rs"),
                comment(502, Some(901), 12, "src/lib.rs"),
            ],
            resolved: vec![501],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "only the unresolved comment's review is queued"
        );
        assert!(tasks[0].key.contains("#12:901"));
    }

    #[test]
    fn jira_issue_becomes_a_research_worktree_keyed_by_issue_key() {
        let jira = MockJira {
            issues: vec![jira_issue("ABC-123", "Fix the retry loop")],
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager_full(MockGh::default(), jira, MockExec::default(), true);

        mgr.poll_once();
        mgr.drive_queue();

        let created = exec.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "ABC-123", "the Jira key is the task id");

        let spawned = exec.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].permission_mode.as_deref(), Some("plan"));
        assert!(spawned[0].prompt.contains("ABC-123"));
        assert!(spawned[0].prompt.contains("RESEARCH.md"));
        assert_eq!(spawned[0].model.as_deref(), Some("sonnet"));
        assert_eq!(
            spawned[0].effort.as_deref(),
            Some("high"),
            "research is a lighter first pass than a human-facing reply"
        );
    }

    #[test]
    fn jira_stays_dormant_without_credentials() {
        let jira = MockJira {
            issues: vec![jira_issue("ABC-123", "Fix the retry loop")],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager_full(MockGh::default(), jira, MockExec::default(), false);
        mgr.poll_once();
        assert_eq!(mgr.list_tasks().unwrap().len(), 0);
        assert!(!mgr.status().jira_configured);
    }

    #[test]
    fn a_jira_failure_does_not_block_github_polling() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let jira = MockJira {
            fails: true,
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager_full(gh, jira, MockExec::default(), true);
        mgr.poll_once();

        assert_eq!(
            mgr.list_tasks().unwrap().len(),
            1,
            "the GitHub task is queued despite the Jira error"
        );
        let error = mgr.status().last_error.expect("error is surfaced");
        assert!(error.contains("Jira"), "the error names the failing side");
    }

    #[test]
    fn usage_gate_holds_the_queue_until_the_window_clears() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
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
            pulls: vec![
                review_request(12, "Add retry", "feature/retry"),
                review_request(13, "Add docs", "feature/docs"),
            ],
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

    #[test]
    fn a_finished_pr_comment_task_raises_an_attention_item() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![comment(501, Some(900), 12, "src/retry.rs")],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, exec, bus) = manager(gh, exec);
        let mut rx = bus.subscribe();
        mgr.poll_once();
        mgr.drive_queue();
        assert_eq!(exec.spawned.lock().unwrap().len(), 1);

        mgr.on_session_finished("sess-1", true);

        let mut saw_it = false;
        while let Ok(event) = rx.try_recv() {
            if let Event::AttentionRequired { source, branch, .. } = event {
                assert_eq!(source, "pr_review_ready");
                assert_eq!(branch.as_deref(), Some("impl/T-1-x"));
                saw_it = true;
            }
        }
        assert!(saw_it, "a finished pr_comment task should raise attention");
    }

    #[test]
    fn a_failed_pr_comment_task_does_not_raise_attention() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![comment(501, Some(900), 12, "src/retry.rs")],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, _exec, bus) = manager(gh, exec);
        let mut rx = bus.subscribe();
        mgr.poll_once();
        mgr.drive_queue();

        mgr.on_session_finished("sess-1", false);

        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, Event::AttentionRequired { .. }),
                "a failed plan is nothing to review"
            );
        }
    }

    #[test]
    fn legacy_issue_tasks_are_dismissed_not_crashed() {
        let (mgr, exec, _bus) = manager(MockGh::default(), MockExec::default());
        let now = Utc::now();
        mgr.store
            .insert_daemon_task(&DaemonTask {
                key: "issue:owner/repo#7".into(),
                kind: "issue".into(),
                state: "queued".into(),
                title: "#7 Old flow".into(),
                payload: "{}".into(),
                branch: None,
                session_id: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        mgr.drive_queue();
        assert!(exec.spawned.lock().unwrap().is_empty());
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks[0].state, "dismissed");
    }

    #[tokio::test]
    async fn the_queue_advances_on_awaiting_input_without_the_session_being_closed() {
        let gh = MockGh {
            pulls: vec![
                review_request(12, "Add retry", "feature/retry"),
                review_request(13, "Add docs", "feature/docs"),
            ],
            ..Default::default()
        };
        let (mgr, exec, bus) = manager(gh, MockExec::default());
        mgr.poll_once();
        mgr.drive_queue();
        assert_eq!(exec.spawned.lock().unwrap().len(), 1, "first task started");

        let loop_handle = tokio::spawn(mgr.clone().run_loop(bus.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // The session's turn ends — conversational plan-mode sessions go
        // "awaiting_input", not "done", and nobody is going to close them.
        bus.publish(Event::SessionStatusChanged {
            session_id: "sess-1".into(),
            branch: "feature/retry".into(),
            status: SessionStatus::AwaitingInput,
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        loop_handle.abort();

        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.iter().filter(|t| t.state == "done").count(),
            1,
            "awaiting_input alone must finish the task"
        );
        assert_eq!(
            exec.spawned.lock().unwrap().len(),
            2,
            "the second task starts without anyone closing the first session"
        );
    }
}
