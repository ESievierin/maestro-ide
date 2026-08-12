//! Background daemon (Этап 3): watches GitHub and Jira for work addressed to
//! the user and turns it into prepared, human-gated sessions.
//!
//! Three flows, all deliberately read-only:
//! - **review requested on a PR** (the configured account is in
//!   `requested_reviewers`) → fetch the PR branch, get a worktree on it, and
//!   run a plan-mode session that reviews the diff and calls
//!   `submit_review_comments` with its draft findings, one per file+line.
//!   Re-triggers when the PR head moves.
//! - **new review comment on a PR whose head branch has a worktree here**
//!   (someone reviewing *your* work) → a read-only session that verifies the
//!   comment against the diff and calls `submit_review_comments` with a
//!   draft reply per comment.
//! - **Jira issue assigned to you** (only when `jira_base_url`/`jira_email`/
//!   `jira_token` are configured) → research worktree + a plan-mode session
//!   writing `RESEARCH.md`.
//!
//! Nothing is ever posted to GitHub/Jira and nothing is committed — every
//! `submit_review_comments` call raises a dialog the human approves (and can
//! edit or drop any entry from) before anything is posted; acting on
//! anything else prepared here is the human's move through the ordinary
//! (gated) session flow.
//!
//! Off by default (`daemon_enabled`). One task runs at a time; new work waits
//! in a persistent queue that survives restarts without duplicating anything
//! (task keys are idempotent). A usage gate keeps the background lane from
//! eating the 5-hour window interactive work needs.

pub mod github;
pub mod jira;

pub use github::{GhAccount, GhCli, GhProvider, NewReviewComment};
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
/// JSON array of gh logins the daemon polls on behalf of, in addition to (or
/// instead of listing) the single `daemon_account` posting identity. Empty =
/// just that one account — today's single-account behavior, unchanged.
pub const SETTING_DAEMON_WATCH_ACCOUNTS: &str = "daemon_watch_accounts";
/// JSON array of label names (case-insensitive); a PR carrying any of them is
/// skipped entirely — no review-request task, no comment-reply task. Empty =
/// today's unfiltered behavior. For real teams marking PRs "wip"/"draft"/
/// "do-not-review" that the daemon has no business preparing research on yet.
pub const SETTING_DAEMON_SKIP_LABELS: &str = "daemon_skip_labels";

const DEFAULT_POLL_MINUTES: u64 = 5;
const DEFAULT_USAGE_THRESHOLD: f64 = 50.0;
/// Research is a first pass over unfamiliar ground — a capable model is enough.
const DEFAULT_RESEARCH_MODEL: &str = "sonnet";
const DEFAULT_RESEARCH_EFFORT: &str = "high";
/// Verify produces text a human reviewer will actually read (a PR review, a
/// reply to a comment) — worth the extra reasoning.
const DEFAULT_VERIFY_MODEL: &str = "sonnet";
const DEFAULT_VERIFY_EFFORT: &str = "xhigh";
/// Total start attempts allowed for a task before a transient failure gives up
/// and falls back to `failed` — 30s poll-tick spacing is the backoff, no timer needed.
const MAX_DAEMON_TASK_ATTEMPTS: u32 = 3;

/// What the frontend chip/panel shows.
#[derive(Clone, Debug, Serialize)]
pub struct DaemonStatus {
    pub enabled: bool,
    /// The account the daemon is configured to act as (resolved, may be empty)
    /// — used for posting/replying elsewhere in the app (unaffected by
    /// `watched_accounts`, which is purely about detection).
    pub account: String,
    /// All gh logins on this machine, for the account picker.
    pub accounts: Vec<GhAccount>,
    /// Every account the daemon polls on behalf of this cycle — always
    /// includes at least `account` when resolvable.
    pub watched_accounts: Vec<String>,
    /// Label names that make the daemon skip a PR entirely. Empty = no filter.
    pub skip_labels: Vec<String>,
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
    /// Get-or-create the branch's persistent Main session, without sending it
    /// anything yet — used right after a worktree is created so the agent is
    /// there and ready before the first real task arrives. `model`/`effort` seed
    /// a fresh spawn only; they're ignored when a live Main session already exists.
    fn ensure_main_session(
        &self,
        branch: &str,
        cwd: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()>;
    /// Get-or-create the branch's Main session and send it `prompt`. Returns the
    /// Main session's id, for `daemon_task` bookkeeping.
    fn send_to_main(
        &self,
        branch: &str,
        cwd: &str,
        prompt: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<String>;
    /// The `review-reply-style` voice guide, or an empty string if none is configured.
    fn reply_style_guide(&self) -> String;
    /// The `review-workflow-gate` guide ("discuss first"), or an empty string if none is
    /// configured.
    fn workflow_gate_guide(&self) -> String;
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

    fn ensure_main_session(
        &self,
        branch: &str,
        cwd: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        self.sessions.ensure_main(branch, cwd, model, effort)?;
        Ok(())
    }

    fn send_to_main(
        &self,
        branch: &str,
        cwd: &str,
        prompt: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<String> {
        let session = self.sessions.ensure_main(branch, cwd, model, effort)?;
        self.sessions.send(&session.id, prompt, &[])?;
        Ok(session.id)
    }

    fn reply_style_guide(&self) -> String {
        self.sessions.render_reply_style()
    }

    fn workflow_gate_guide(&self) -> String {
        self.sessions.render_workflow_gate()
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

    /// Every account the daemon polls on behalf of, in order — the
    /// `daemon_watch_accounts` setting, filtered to logins `gh` still knows
    /// about (a since-logged-out account is dropped, not an error); falls
    /// back to just the single posting identity when unset or empty, which
    /// is the exact single-account behavior this setting did not used to
    /// exist to change.
    fn watched_accounts(&self, accounts: &[GhAccount]) -> Vec<String> {
        let configured: Vec<String> = self
            .setting(SETTING_DAEMON_WATCH_ACCOUNTS)
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|login| accounts.iter().any(|a| &a.login == login))
            .collect();
        if !configured.is_empty() {
            return configured;
        }
        let primary = self.account(accounts);
        if primary.is_empty() {
            Vec::new()
        } else {
            vec![primary]
        }
    }

    /// Replace the watch list. An empty list is valid — it just means "fall
    /// back to the single posting identity" (see [`Self::watched_accounts`]).
    pub fn set_watched_accounts(&self, accounts: &[String]) -> Result<()> {
        let json = serde_json::to_string(accounts).map_err(|err| MaestroError::InvalidData {
            message: format!("could not serialize watched accounts: {err}"),
        })?;
        self.store
            .set_setting(SETTING_DAEMON_WATCH_ACCOUNTS, &json)?;
        self.bus.publish(Event::DaemonUpdated {});
        Ok(())
    }

    /// The `daemon_skip_labels` setting — empty when unset, meaning nothing
    /// is filtered (today's behavior, unchanged).
    fn skip_labels(&self) -> Vec<String> {
        self.setting(SETTING_DAEMON_SKIP_LABELS)
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    /// Replace the skip-label list. An empty list clears filtering entirely.
    pub fn set_skip_labels(&self, labels: &[String]) -> Result<()> {
        let json = serde_json::to_string(labels).map_err(|err| MaestroError::InvalidData {
            message: format!("could not serialize skip labels: {err}"),
        })?;
        self.store.set_setting(SETTING_DAEMON_SKIP_LABELS, &json)?;
        self.bus.publish(Event::DaemonUpdated {});
        Ok(())
    }

    pub fn status(&self) -> DaemonStatus {
        let accounts = self.gh.accounts().unwrap_or_default();
        let account = self.account(&accounts);
        let watched_accounts = self.watched_accounts(&accounts);
        let tasks = self.store.list_daemon_tasks().unwrap_or_default();
        let queued = tasks.iter().filter(|t| t.state == "queued").count();
        let running = tasks.into_iter().find(|t| t.state == "running");
        let runtime = self.runtime.lock().ok();
        DaemonStatus {
            enabled: self.enabled(),
            account,
            accounts,
            watched_accounts,
            skip_labels: self.skip_labels(),
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

    /// Comment ids already folded into some earlier `pr_comment` task for this PR —
    /// the guard against re-bundling a comment a previous poll already queued just
    /// because a newer, unrelated comment landed alongside it this time.
    fn already_queued_comment_ids(&self, slug: &str, pr: u64) -> std::collections::HashSet<u64> {
        let prefix = format!("pr-comment:{slug}#{pr}:");
        let mut ids = std::collections::HashSet::new();
        let Ok(tasks) = self.list_tasks() else {
            return ids;
        };
        for task in tasks {
            if task.kind != "pr_comment" || !task.key.starts_with(&prefix) {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&task.payload) else {
                continue;
            };
            let Some(comments) = payload.get("comments").and_then(|c| c.as_array()) else {
                continue;
            };
            for comment in comments {
                if let Some(id) = comment.get("comment_id").and_then(|v| v.as_u64()) {
                    ids.insert(id);
                }
            }
        }
        ids
    }

    fn poll_github(&self) -> Result<()> {
        let accounts = self.gh.accounts()?;
        let watch_list = self.watched_accounts(&accounts);
        if watch_list.is_empty() {
            return Err(MaestroError::Config {
                message: "no gh account available — log in with `gh auth login` and pick one in the daemon panel".into(),
            });
        }
        let slug = self.watched_repo()?;
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.repo = Some(slug.clone());
        }

        // A PR's contents (open list, comments, resolved threads) are repo-scoped,
        // not account-scoped — one working token is enough to see all of it. Only
        // "am I a requested reviewer" / "is this comment mine" need to check every
        // watched login, not just whichever one happened to authenticate.
        let mut tokens: Vec<String> = Vec::new();
        for account in &watch_list {
            match self.gh.token(account) {
                Ok(token) => tokens.push(token),
                Err(err) => {
                    tracing::warn!(account, error = %err, "no token for a watched account; skipping it this poll");
                }
            }
        }
        let Some(primary_token) = tokens.into_iter().next() else {
            return Err(MaestroError::Config {
                message: "no token available for any watched gh account".into(),
            });
        };

        let mut new_tasks = 0usize;
        let skip_labels: Vec<String> = self
            .skip_labels()
            .iter()
            .map(|l| l.to_lowercase())
            .collect();

        for pull in self.gh.open_pulls(&primary_token, &slug)? {
            if !skip_labels.is_empty()
                && pull
                    .labels
                    .iter()
                    .any(|l| skip_labels.contains(&l.to_lowercase()))
            {
                continue;
            }

            // Someone wants one of our watched accounts' review → prepare one per
            // matching account, re-keyed per head SHA so a new push produces a
            // fresh review pass. The key only carries the account when there is
            // more than one watched — the common single-account case keeps the
            // exact key format from before this setting existed, so an existing
            // queued/done task for an open PR is never re-queued as a duplicate
            // on upgrade.
            for account in &watch_list {
                if pull.requested_reviewers.iter().any(|r| r == account) {
                    let key = if watch_list.len() > 1 {
                        format!(
                            "pr-review:{account}:{slug}#{}:{}",
                            pull.number, pull.head_sha
                        )
                    } else {
                        format!("pr-review:{slug}#{}:{}", pull.number, pull.head_sha)
                    };
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
            }

            // Comments only matter on PRs whose head branch we work on here.
            if self.exec.worktree_for_branch(&pull.head_ref)?.is_none() {
                continue;
            }
            let resolved = self
                .gh
                .resolved_comment_ids(&primary_token, &slug, pull.number)
                .unwrap_or_else(|err| {
                    tracing::debug!(error = %err, pr = pull.number, "could not check resolved threads; treating all comments as unresolved");
                    Default::default()
                });
            let already_queued = self.already_queued_comment_ids(&slug, pull.number);
            // One task per poll, not per review: whatever is newly visible on this
            // pass — regardless of which review submission it came from — gets
            // bundled into one plan. A reviewer who leaves 4 comments across two
            // separate review submissions still gets one task covering all 4, and
            // a comment already folded into an earlier task never reappears in a
            // later one just because a *different* comment landed in between.
            let mut new_comments: Vec<github::GhComment> = self
                .gh
                .pull_comments(&primary_token, &slug, pull.number)?
                .into_iter()
                .filter(|c| !resolved.contains(&c.id))
                // Our own replies (from *any* watched account) come back through
                // this same endpoint on the next poll — without this they would
                // look like a fresh comment to react to.
                .filter(|c| !watch_list.iter().any(|a| a == &c.author))
                .filter(|c| !already_queued.contains(&c.id))
                .collect();
            if !new_comments.is_empty() {
                new_comments.sort_by_key(|c| c.id);
                let key = format!(
                    "pr-comment:{slug}#{}:{}",
                    pull.number,
                    new_comments
                        .iter()
                        .map(|c| c.id.to_string())
                        .collect::<Vec<_>>()
                        .join("-")
                );
                let payload = serde_json::json!({
                    "pr": pull.number,
                    "pr_title": pull.title,
                    "head_ref": pull.head_ref,
                    "comments": new_comments.iter().map(|c| serde_json::json!({
                        "comment_id": c.id,
                        "review_id": c.review_id,
                        "author": c.author,
                        "path": c.path,
                        "body": c.body,
                        "url": c.url,
                    })).collect::<Vec<_>>(),
                });
                let authors = new_comments
                    .iter()
                    .map(|c| c.author.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                let title = format!(
                    "PR #{} — {} review comment{} ({authors})",
                    pull.number,
                    new_comments.len(),
                    if new_comments.len() == 1 { "" } else { "s" },
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
            attempts: 0,
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
            if err.is_transient() && task.attempts + 1 < MAX_DAEMON_TASK_ATTEMPTS {
                let _ = self.store.requeue_daemon_task_for_retry(&task.key);
                tracing::warn!(
                    key = %task.key,
                    attempt = task.attempts + 1,
                    error = %err,
                    "daemon task failed to start, will retry"
                );
            } else {
                let _ = self
                    .store
                    .update_daemon_task(&task.key, "failed", None, None);
                crate::error::report(&self.bus, &err);
                tracing::warn!(key = %task.key, error = %err, "daemon task failed to start");
            }
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

        let mut prompt = format!(
            "Your review was requested on PR #{pr} by {author}.\n\n\
             Title: {pr_title}\n{url}\n\nPR description:\n{pr_body}\n\n\
             This worktree has the PR branch checked out. Review the changes this branch \
             introduces relative to its base (use read-only git commands like \
             `git log`/`git diff` against the base branch). Do NOT modify any files, do \
             NOT commit, and do NOT post anything to GitHub directly — submit_review_comments \
             is the only way feedback reaches GitHub, one entry per finding (path, line, \
             body), ordered by severity."
        );
        let gate = self.exec.workflow_gate_guide();
        if !gate.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&gate);
        }
        // Reviewing someone else's PR happens in that PR's own worktree's Main
        // session — persistent, so a re-review after the head moves continues the
        // same conversation instead of starting cold every time.
        let (model, effort) = self.verify_model_effort();
        let session_id = self
            .exec
            .send_to_main(&branch, &cwd, &prompt, model, effort)?;

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
        self.exec.ensure_main_session(&branch, &cwd, None, None)?;

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
             and correct. Do NOT modify any other files, do NOT commit, and do NOT post \
             anything to GitHub directly — submit_review_comments is the only way a reply \
             reaches GitHub: one entry per comment you want to reply to, in_reply_to set to \
             that comment's id (the number in \"[comment N]\"), path/line matching that same \
             comment's own, body your draft reply.",
        );
        let gate = self.exec.workflow_gate_guide();
        if !gate.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&gate);
        }
        let style = self.exec.reply_style_guide();
        if !style.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&style);
        }
        // Comments on your own PR go to that branch's Main session — the exact
        // session PrRepliesDialog itself resumes, so the human sees the same
        // ongoing conversation whichever side triggered it.
        let (model, effort) = self.verify_model_effort();
        let session_id = self
            .exec
            .send_to_main(&branch, &cwd, &prompt, model, effort)?;

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
    use crate::error::GitErrorKind;
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

    enum SpawnFailure {
        Transient,
        Permanent,
    }

    /// `(branch, cwd, prompt, model, effort)` as recorded by `MockExec::send_to_main`.
    type MainSentCall = (String, String, String, Option<String>, Option<String>);

    #[derive(Default)]
    struct MockExec {
        worktrees: Vec<String>,
        created: StdMutex<Vec<(String, String)>>,
        fetched: StdMutex<Vec<String>>,
        spawned: StdMutex<Vec<SpawnParams>>,
        /// (branch, cwd) recorded by `ensure_main_session`.
        main_ensured: StdMutex<Vec<(String, String)>>,
        main_sent: StdMutex<Vec<MainSentCall>>,
        fail_spawn: Option<SpawnFailure>,
        reply_style: String,
        workflow_gate: String,
    }

    impl MockExec {
        fn maybe_fail(&self) -> Result<()> {
            match self.fail_spawn {
                Some(SpawnFailure::Transient) => Err(MaestroError::Git {
                    kind: GitErrorKind::CommandFailed,
                    message: "simulated transient git failure".into(),
                }),
                Some(SpawnFailure::Permanent) => Err(MaestroError::Config {
                    message: "simulated permanent failure".into(),
                }),
                None => Ok(()),
            }
        }
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
            self.maybe_fail()?;
            let id = format!("sess-{}", self.spawned.lock().unwrap().len() + 1);
            self.spawned.lock().unwrap().push(params);
            Ok(id)
        }
        fn ensure_main_session(
            &self,
            branch: &str,
            cwd: &str,
            _model: Option<String>,
            _effort: Option<String>,
        ) -> Result<()> {
            self.maybe_fail()?;
            self.main_ensured
                .lock()
                .unwrap()
                .push((branch.to_string(), cwd.to_string()));
            Ok(())
        }
        fn send_to_main(
            &self,
            branch: &str,
            cwd: &str,
            prompt: &str,
            model: Option<String>,
            effort: Option<String>,
        ) -> Result<String> {
            self.maybe_fail()?;
            let id = format!("main-{}", self.main_sent.lock().unwrap().len() + 1);
            self.main_sent.lock().unwrap().push((
                branch.to_string(),
                cwd.to_string(),
                prompt.to_string(),
                model,
                effort,
            ));
            Ok(id)
        }
        fn reply_style_guide(&self) -> String {
            self.reply_style.clone()
        }
        fn workflow_gate_guide(&self) -> String {
            self.workflow_gate.clone()
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
            labels: Vec::new(),
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
        // Reviewing someone else's PR now goes to that worktree's Main session
        // instead of a fresh spawn — nothing new is spawned here at all.
        assert!(exec.spawned.lock().unwrap().is_empty());
        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (branch, _cwd, prompt, model, effort) = &sent[0];
        assert_eq!(branch, "feature/retry");
        assert!(prompt.contains("submit_review_comments"));
        assert!(prompt.contains("Add retry"));
        assert!(prompt.contains("do NOT post anything"));
        assert_eq!(
            model.as_deref(),
            Some("sonnet"),
            "the verify bucket defaults to sonnet"
        );
        assert_eq!(
            effort.as_deref(),
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

        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent[0].3.as_deref(), Some("opus"));
        assert_eq!(sent[0].4.as_deref(), Some("medium"));
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
    fn watching_a_second_account_catches_a_review_request_the_first_would_miss() {
        // MockGh's accounts are "personal" (active/default) and "work". A PR that
        // only asks "work" to review is invisible with just the default account watched.
        let gh = MockGh {
            pulls: vec![GhPull {
                requested_reviewers: vec!["work".into()],
                ..pull(12, "Add retry", "feature/retry")
            }],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.poll_once();
        assert_eq!(
            mgr.list_tasks().unwrap().len(),
            0,
            "only 'personal' is watched by default; 'work' being asked is not our concern yet"
        );

        mgr.set_watched_accounts(&["personal".to_string(), "work".to_string()])
            .unwrap();
        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "now watching 'work' too, the request is caught"
        );
        assert!(tasks[0].key.contains("work"), "key: {}", tasks[0].key);
    }

    #[test]
    fn two_watched_accounts_both_requested_produce_two_distinct_tasks() {
        let gh = MockGh {
            pulls: vec![GhPull {
                requested_reviewers: vec!["personal".into(), "work".into()],
                ..pull(12, "Add retry", "feature/retry")
            }],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.set_watched_accounts(&["personal".to_string(), "work".to_string()])
            .unwrap();

        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "each watched account being asked to review is its own task"
        );
        let keys: std::collections::HashSet<&str> = tasks.iter().map(|t| t.key.as_str()).collect();
        assert!(keys.iter().any(|k| k.contains("personal")));
        assert!(keys.iter().any(|k| k.contains("work")));
    }

    #[test]
    fn a_pr_carrying_a_skip_label_gets_no_review_task_at_all() {
        let gh = MockGh {
            pulls: vec![GhPull {
                requested_reviewers: vec!["personal".into()],
                labels: vec!["WIP".into()],
                ..pull(12, "Add retry", "feature/retry")
            }],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.set_skip_labels(&["wip".to_string()]).unwrap();
        mgr.poll_once();
        assert_eq!(
            mgr.list_tasks().unwrap().len(),
            0,
            "a skip-labeled PR must produce no task even though it requests our review"
        );
    }

    #[test]
    fn skip_labels_match_case_insensitively_but_not_a_different_label() {
        let gh = MockGh {
            pulls: vec![
                GhPull {
                    requested_reviewers: vec!["personal".into()],
                    labels: vec!["Draft".into()],
                    ..pull(12, "Add retry", "feature/retry")
                },
                GhPull {
                    requested_reviewers: vec!["personal".into()],
                    labels: vec!["bug".into()],
                    ..pull(13, "Fix crash", "feature/fix-crash")
                },
            ],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        mgr.set_skip_labels(&["draft".to_string()]).unwrap();
        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "'Draft' (any case) is skipped, 'bug' is untouched"
        );
        assert!(tasks[0].key.contains("13"), "key: {}", tasks[0].key);
    }

    #[test]
    fn empty_skip_labels_filters_nothing() {
        let gh = MockGh {
            pulls: vec![GhPull {
                requested_reviewers: vec!["personal".into()],
                labels: vec!["wip".into()],
                ..pull(12, "Add retry", "feature/retry")
            }],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        // No set_skip_labels call at all — must behave exactly as before this setting existed.
        mgr.poll_once();
        assert_eq!(mgr.list_tasks().unwrap().len(), 1);
    }

    #[test]
    fn a_reply_from_any_watched_account_is_never_treated_as_new_feedback() {
        // Comment 501 is a genuine incoming comment; 502 is "our" reply from the
        // *second* watched account — must be filtered out same as if it were the
        // default account's own reply (the bug T1 of this session's PR-workflow
        // fixes addressed, now checked across every watched identity, not just one).
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![
                comment(501, Some(900), 12, "src/retry.rs"),
                github::GhComment {
                    author: "work".into(),
                    ..comment(502, Some(900), 12, "src/retry.rs")
                },
            ],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, exec);
        mgr.set_watched_accounts(&["personal".to_string(), "work".to_string()])
            .unwrap();

        mgr.poll_once();
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&tasks[0].payload).unwrap();
        let comment_ids: Vec<u64> = payload["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["comment_id"].as_u64().unwrap())
            .collect();
        assert_eq!(comment_ids, vec![501], "502 is our own reply from 'work'");
    }

    #[test]
    fn a_stale_watched_account_no_longer_authenticated_is_dropped_not_errored() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(gh, MockExec::default());
        // "ghost" was logged out of gh since this was configured; MockGh only
        // knows "personal" and "work".
        mgr.set_watched_accounts(&["ghost".to_string()]).unwrap();

        mgr.poll_once();
        let status = mgr.status();
        assert!(
            status.last_error.is_none(),
            "a stale account falls back to the default, not an error: {:?}",
            status.last_error
        );
        // Falls back to the single posting identity ("personal", the active account).
        assert_eq!(mgr.list_tasks().unwrap().len(), 1);
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
        assert!(tasks[0].key.contains("#12:501"));

        mgr.drive_queue();
        // Comments on your own PR go to that branch's Main session, not a fresh spawn.
        assert!(exec.spawned.lock().unwrap().is_empty());
        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "impl/T-1-x");
        assert!(sent[0].2.contains("submit_review_comments"));
        assert!(sent[0].2.contains("do NOT post anything"));
    }

    #[test]
    fn a_configured_reply_style_guide_is_appended_to_the_comment_prompt() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![comment(501, Some(900), 12, "src/retry.rs")],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            reply_style: "MARKER: match the reviewer's language".into(),
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        mgr.drive_queue();

        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].2.contains("MARKER: match the reviewer's language"));
    }

    #[test]
    fn a_configured_workflow_gate_is_appended_to_the_comment_prompt() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            comments: vec![comment(501, Some(900), 12, "src/retry.rs")],
            ..Default::default()
        };
        let exec = MockExec {
            worktrees: vec!["impl/T-1-x".into()],
            workflow_gate: "MARKER: wait for an explicit go-ahead".into(),
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        mgr.drive_queue();

        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].2.contains("MARKER: wait for an explicit go-ahead"));
    }

    #[test]
    fn a_configured_workflow_gate_is_appended_to_the_review_prompt() {
        let gh = MockGh {
            pulls: vec![review_request(12, "Add retry", "feature/retry")],
            ..Default::default()
        };
        let exec = MockExec {
            workflow_gate: "MARKER: wait for an explicit go-ahead".into(),
            ..Default::default()
        };
        let (mgr, exec, _bus) = manager(gh, exec);

        mgr.poll_once();
        mgr.drive_queue();

        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].2.contains("MARKER: wait for an explicit go-ahead"));
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
        assert!(tasks[0].key.contains("#12:501-502-503"));

        mgr.drive_queue();
        let sent = exec.main_sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // All three comments made it into the one prompt.
        assert!(sent[0].2.contains("[comment 501]"));
        assert!(sent[0].2.contains("[comment 502]"));
        assert!(sent[0].2.contains("[comment 503]"));
        assert!(sent[0].2.contains("## src/retry.rs"));
        assert!(sent[0].2.contains("## src/lib.rs"));
    }

    #[test]
    fn comments_from_different_reviews_detected_together_become_one_task() {
        // The old design grouped strictly by review submission; the new one groups
        // by detection pass instead — two comments from two different reviews,
        // both new on the same poll, still land in one task.
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
        assert_eq!(tasks.len(), 1, "detected in the same pass, one task");
        assert!(tasks[0].key.contains("#12:501-601"));
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
        assert_eq!(tasks.len(), 1, "only the unresolved comment is queued");
        assert!(tasks[0].key.contains("#12:502"));
    }

    #[test]
    fn our_own_replies_never_trigger_a_task() {
        let gh = MockGh {
            pulls: vec![pull(12, "Add retry", "impl/T-1-x")],
            // MockGh::accounts() resolves the active account to "personal" —
            // a comment authored by that same login is our own reply coming
            // back through the same endpoint, not incoming feedback.
            comments: vec![
                GhComment {
                    author: "personal".into(),
                    ..comment(501, Some(900), 12, "src/retry.rs")
                },
                comment(502, Some(900), 12, "src/retry.rs"),
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
        assert_eq!(tasks.len(), 1, "only the reviewer's comment is queued");
        assert!(tasks[0].key.contains("#12:502"));
        assert!(!tasks[0].key.contains("501"));
    }

    #[test]
    fn a_comment_already_covered_by_an_earlier_task_is_not_bundled_again() {
        // 501 was already folded into a task from an earlier poll; 502 is new.
        // A later poll (simulated directly: insert the earlier task, then ask
        // what's already covered) must report only 501 as covered, not 502 —
        // the guard that stops a later poll from re-bundling an old comment
        // just because something new landed alongside it.
        let (mgr, _exec, _bus) = manager(MockGh::default(), MockExec::default());
        let now = Utc::now();
        mgr.store
            .insert_daemon_task(&DaemonTask {
                key: "pr-comment:owner/repo#12:501".into(),
                kind: "pr_comment".into(),
                state: "done".into(),
                title: "PR #12 — 1 review comment (reviewer)".into(),
                payload: serde_json::json!({
                    "pr": 12,
                    "comments": [{"comment_id": 501, "author": "reviewer"}],
                })
                .to_string(),
                branch: Some("impl/T-1-x".into()),
                session_id: None,
                attempts: 0,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        let covered = mgr.already_queued_comment_ids("owner/repo", 12);
        assert_eq!(covered, [501].into_iter().collect());
        assert!(!covered.contains(&502));
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

        let ensured = exec.main_ensured.lock().unwrap();
        assert_eq!(
            ensured.len(),
            1,
            "the research worktree gets its own main agent eagerly, ready for later"
        );

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
            exec.main_sent.lock().unwrap().is_empty(),
            "80% utilization is over the 50% default threshold"
        );
        let task = mgr.store.next_queued_daemon_task().unwrap();
        assert!(task.is_some(), "the task waits, it is not lost");

        mgr.set_utilization_for_test(Some(10.0));
        mgr.drive_queue();
        assert_eq!(exec.main_sent.lock().unwrap().len(), 1);
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
        assert_eq!(exec.main_sent.lock().unwrap().len(), 1);

        mgr.on_session_finished("main-1", true);
        // Finishing the first task pulls the second one in.
        assert_eq!(exec.main_sent.lock().unwrap().len(), 2);
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
        assert_eq!(exec.main_sent.lock().unwrap().len(), 1);

        mgr.on_session_finished("main-1", true);

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

        mgr.on_session_finished("main-1", false);

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
                attempts: 0,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        mgr.drive_queue();
        assert!(exec.spawned.lock().unwrap().is_empty());
        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(tasks[0].state, "dismissed");
    }

    fn pr_comment_task(key: &str, head_ref: &str, attempts: u32) -> DaemonTask {
        let now = Utc::now();
        DaemonTask {
            key: key.into(),
            kind: "pr_comment".into(),
            state: "queued".into(),
            title: "PR #9 — 1 review comment (reviewer)".into(),
            payload: serde_json::json!({
                "pr": 9,
                "head_ref": head_ref,
                "comments": [{
                    "comment_id": 501,
                    "author": "reviewer",
                    "body": "fix this",
                    "path": "src/lib.rs",
                    "url": "https://github.com/owner/repo/pull/9#discussion_r501",
                }],
            })
            .to_string(),
            branch: None,
            session_id: None,
            attempts,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_transient_start_failure_is_requeued_for_retry_not_failed() {
        let exec = MockExec {
            worktrees: vec!["feature/retry".into()],
            fail_spawn: Some(SpawnFailure::Transient),
            ..Default::default()
        };
        let (mgr, _exec, bus) = manager(MockGh::default(), exec);
        let mut rx = bus.subscribe();
        mgr.store
            .insert_daemon_task(&pr_comment_task(
                "pr-comment:owner/repo#9:501",
                "feature/retry",
                0,
            ))
            .unwrap();

        mgr.drive_queue();

        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks[0].state, "queued",
            "a transient failure should be retried, not failed outright"
        );
        assert_eq!(tasks[0].attempts, 1);
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, Event::ErrorRaised { .. }),
                "no error should surface to the user while retries remain"
            );
        }
    }

    #[test]
    fn a_transient_failure_gives_up_after_max_attempts() {
        let exec = MockExec {
            worktrees: vec!["feature/retry".into()],
            fail_spawn: Some(SpawnFailure::Transient),
            ..Default::default()
        };
        let (mgr, _exec, bus) = manager(MockGh::default(), exec);
        let mut rx = bus.subscribe();
        mgr.store
            .insert_daemon_task(&pr_comment_task(
                "pr-comment:owner/repo#9:501",
                "feature/retry",
                MAX_DAEMON_TASK_ATTEMPTS - 1,
            ))
            .unwrap();

        mgr.drive_queue();

        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks[0].state, "failed",
            "once the attempt budget is spent, a still-transient failure must fail for real"
        );
        let mut saw_error = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, Event::ErrorRaised { .. }) {
                saw_error = true;
            }
        }
        assert!(saw_error, "the final failure should surface to the user");
    }

    #[test]
    fn a_permanent_start_failure_is_never_retried() {
        let exec = MockExec {
            worktrees: vec!["feature/retry".into()],
            fail_spawn: Some(SpawnFailure::Permanent),
            ..Default::default()
        };
        let (mgr, _exec, _bus) = manager(MockGh::default(), exec);
        mgr.store
            .insert_daemon_task(&pr_comment_task(
                "pr-comment:owner/repo#9:501",
                "feature/retry",
                0,
            ))
            .unwrap();

        mgr.drive_queue();

        let tasks = mgr.list_tasks().unwrap();
        assert_eq!(
            tasks[0].state, "failed",
            "a permanent-looking error (bad config, etc.) must not be retried"
        );
        assert_eq!(
            tasks[0].attempts, 0,
            "attempts only bump on an actual retry"
        );
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
        assert_eq!(
            exec.main_sent.lock().unwrap().len(),
            1,
            "first task started"
        );

        let loop_handle = tokio::spawn(mgr.clone().run_loop(bus.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // The session's turn ends — conversational plan-mode sessions go
        // "awaiting_input", not "done", and nobody is going to close them.
        bus.publish(Event::SessionStatusChanged {
            session_id: "main-1".into(),
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
            exec.main_sent.lock().unwrap().len(),
            2,
            "the second task starts without anyone closing the first session"
        );
    }
}
