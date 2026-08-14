//! Attention queue (T9).
//!
//! One place that answers "who needs me?". Everything that can block a fleet of agents
//! — an inline permission request, a gated commit/push, a failed session, a line-question
//! answer that arrived while the user was elsewhere — becomes an [`AttentionItem`] here.
//!
//! The queue is derived purely from bus events, so it stays correct no matter which
//! module produced the situation, and it publishes `attention.updated` when it changes:
//! panels refetch, nothing is pushed into them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::session::SessionStatus;
use crate::error::{MaestroError, Result};

/// Where the user lands when clicking the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionTarget {
    /// The session's chat tab (inline permission prompt, agent question).
    Chat,
    /// The gate dialog (it is modal, so navigation just means "answer it").
    Gate,
    /// The diff viewer of the branch (line-question answer waiting inline).
    Diff,
    /// The PR-replies dialog for the branch (a daemon review plan is ready).
    PrReplies,
    /// The notes tab of the branch (a red-team's REDTEAM.md is ready to read).
    Notes,
}

/// What kind of situation is waiting. Kept as a string-ish enum so the UI can label and
/// sort without knowing which core module raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    PermissionRequest,
    /// The agent raised a blocking dialog (AskUserQuestion) and cannot continue.
    Question,
    Gate,
    SessionFailed,
    LineQuestion,
    /// A daemon-prepared PR-comment reply plan is awaiting_input, ready to review.
    PrReviewReady,
    /// A red-team session finished its attack pass — REDTEAM.md is ready to send
    /// back to the parent branch.
    RedTeamReady,
}

impl AttentionKind {
    /// Higher sorts first: things that block an agent outrank things that merely
    /// finished, so a fleet of four agents surfaces the blocking one at the top.
    fn priority(&self) -> u8 {
        match self {
            AttentionKind::Gate => 3,
            AttentionKind::Question => 3,
            AttentionKind::PermissionRequest => 2,
            AttentionKind::PrReviewReady => 2,
            AttentionKind::RedTeamReady => 2,
            AttentionKind::SessionFailed => 1,
            AttentionKind::LineQuestion => 0,
        }
    }

    pub fn target(&self) -> AttentionTarget {
        match self {
            AttentionKind::Gate => AttentionTarget::Gate,
            AttentionKind::PermissionRequest
            | AttentionKind::Question
            | AttentionKind::SessionFailed => AttentionTarget::Chat,
            AttentionKind::LineQuestion => AttentionTarget::Diff,
            AttentionKind::PrReviewReady => AttentionTarget::PrReplies,
            // The deliverable is REDTEAM.md, rendered in the Notes tab.
            AttentionKind::RedTeamReady => AttentionTarget::Notes,
        }
    }
}

/// One entry in the queue.
#[derive(Clone, Debug, Serialize)]
pub struct AttentionItem {
    /// Stable id: also the dedup key, so a repeated event does not pile up entries.
    pub id: String,
    pub kind: AttentionKind,
    pub target: AttentionTarget,
    pub branch: Option<String>,
    pub session_id: Option<String>,
    /// One line for the panel.
    pub message: String,
    pub created_at: DateTime<Utc>,
}

/// Setting gating OS notifications (`"true"` enables them).
pub const SETTING_OS_NOTIFICATIONS: &str = "os_notifications";

/// Setting for coalescing notifications that arrive close together into one
/// "N items need you" notification instead of firing one per item (`"true"`
/// enables digesting; the frontend owns the actual debounce/coalescing since
/// that's where notifications are sent).
pub const SETTING_NOTIFICATION_DIGEST: &str = "notification_digest";

pub struct AttentionManager {
    bus: EventBus,
    items: Mutex<HashMap<String, AttentionItem>>,
    /// Sessions whose problems are nobody's business: escalations answer another agent's
    /// question and are closed either way, so a failed one must not nag the user.
    ignored: Mutex<std::collections::HashSet<String>>,
}

impl AttentionManager {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            items: Mutex::new(HashMap::new()),
            ignored: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Never queue anything for this session (used for escalation sessions).
    pub fn ignore_session(&self, session_id: &str) {
        if let Ok(mut ignored) = self.ignored.lock() {
            ignored.insert(session_id.to_string());
        }
    }

    fn is_ignored(&self, session_id: &str) -> bool {
        self.ignored
            .lock()
            .map(|ignored| ignored.contains(session_id))
            .unwrap_or(false)
    }

    /// Queue contents, most urgent first, newest first within a kind.
    pub fn list(&self) -> Result<Vec<AttentionItem>> {
        let items = self.lock()?;
        let mut list: Vec<AttentionItem> = items.values().cloned().collect();
        list.sort_by(|a, b| {
            b.kind
                .priority()
                .cmp(&a.kind.priority())
                .then(b.created_at.cmp(&a.created_at))
        });
        Ok(list)
    }

    /// Drop one item (the user handled or acknowledged it).
    pub fn dismiss(&self, id: &str) -> Result<()> {
        let removed = self.lock()?.remove(id).is_some();
        if removed {
            tracing::debug!(id, "attention item dismissed");
            self.publish_updated();
        }
        Ok(())
    }

    /// Drop everything except gates in one go (stale failures pile up after a
    /// restart). Gates survive: they are questions, not notifications — each
    /// blocks a command until answered in its dialog. Returns how many went.
    pub fn dismiss_all(&self) -> Result<usize> {
        let mut items = self.lock()?;
        let before = items.len();
        items.retain(|_, item| item.kind == AttentionKind::Gate);
        let removed = before - items.len();
        drop(items);
        if removed > 0 {
            tracing::debug!(removed, "attention queue cleared");
            self.publish_updated();
        }
        Ok(removed)
    }

    /// Drop every item of a session (its chat was closed, or it was deleted).
    pub fn dismiss_session(&self, session_id: &str) -> Result<()> {
        let before = self.lock()?.len();
        self.lock()?
            .retain(|_, item| item.session_id.as_deref() != Some(session_id));
        if self.lock()?.len() != before {
            self.publish_updated();
        }
        Ok(())
    }

    /// Consume bus events and keep the queue in sync. Run as a background task.
    pub async fn run_loop(self: Arc<Self>, bus: EventBus) {
        let rx = bus.subscribe();
        self.run_with(rx).await;
    }

    /// Like [`run_loop`](Self::run_loop), but on a receiver subscribed by the
    /// caller — startup subscribes *before* `fail_stale_sessions` publishes its
    /// failures, so "failed on app restart" items land deterministically
    /// instead of racing the spawn of this task.
    pub async fn run_with(self: Arc<Self>, mut rx: tokio::sync::broadcast::Receiver<Event>) {
        loop {
            match rx.recv().await {
                Ok(event) => self.handle(event),
                Err(RecvError::Lagged(skipped)) => {
                    // The queue is derived state; a gap can only leave stale entries,
                    // which the user can still dismiss. Log and carry on.
                    tracing::warn!(skipped, "attention loop lagged behind the bus");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn handle(&self, event: Event) {
        match event {
            // Escalation sessions are bookkeeping between agents: the user never waits on
            // one, so anything it produces (including a failure) stays out of the queue.
            Event::EscalationStarted {
                escalated_session_id,
                ..
            } => {
                self.ignore_session(&escalated_session_id);
                self.remove(&format!("failed:{escalated_session_id}"));
            }
            Event::SessionPermissionRequest {
                session_id,
                request_id,
                tool,
                title,
                ..
            } => {
                if self.is_ignored(&session_id) {
                    return;
                }
                self.add(AttentionItem {
                    id: format!("permission:{request_id}"),
                    kind: AttentionKind::PermissionRequest,
                    target: AttentionKind::PermissionRequest.target(),
                    branch: None,
                    session_id: Some(session_id),
                    message: title.unwrap_or_else(|| format!("{tool} needs permission")),
                    created_at: Utc::now(),
                });
            }
            Event::SessionUserDialog {
                session_id,
                request_id,
                ..
            } => {
                self.add(AttentionItem {
                    id: format!("dialog:{request_id}"),
                    kind: AttentionKind::Question,
                    target: AttentionKind::Question.target(),
                    branch: None,
                    session_id: Some(session_id),
                    message: "The agent is waiting for an answer".to_string(),
                    created_at: Utc::now(),
                });
            }
            Event::SessionUserDialogResolved { request_id, .. } => {
                self.remove(&format!("dialog:{request_id}"));
            }
            Event::GatePending {
                gate_id,
                session_id,
                branch,
                kind,
                ..
            } => {
                self.add(AttentionItem {
                    id: format!("gate:{gate_id}"),
                    kind: AttentionKind::Gate,
                    target: AttentionKind::Gate.target(),
                    branch: Some(branch),
                    session_id: Some(session_id),
                    message: format!("Approval required: {kind}"),
                    created_at: Utc::now(),
                });
            }
            Event::GateResolved { gate_id, .. } => {
                self.remove(&format!("gate:{gate_id}"));
            }
            Event::SessionStatusChanged {
                session_id,
                branch,
                status,
            } => match status {
                SessionStatus::Failed if self.is_ignored(&session_id) => {}
                SessionStatus::Failed => self.add(AttentionItem {
                    id: format!("failed:{session_id}"),
                    kind: AttentionKind::SessionFailed,
                    target: AttentionKind::SessionFailed.target(),
                    branch: Some(branch),
                    session_id: Some(session_id.clone()),
                    message: "Session failed".to_string(),
                    created_at: Utc::now(),
                }),
                // A session that got going again has no outstanding permission prompt.
                SessionStatus::Streaming => self.remove_permissions_of(&session_id),
                _ => {}
            },
            Event::AttentionRequired {
                source,
                branch,
                session_id,
                message,
            } => {
                // Line questions announce completion here; the UI decides whether the
                // user is still looking at that diff. `source` also carries other
                // one-off announcements (the daemon's "plan ready") that need a
                // different kind/target than the line-question default.
                let kind = match source.as_str() {
                    "pr_review_ready" => AttentionKind::PrReviewReady,
                    "red_team_ready" => AttentionKind::RedTeamReady,
                    _ => AttentionKind::LineQuestion,
                };
                let id = match (&session_id, &branch) {
                    (Some(session), _) => format!("{source}:{session}"),
                    (None, Some(branch)) => format!("{source}:{branch}"),
                    _ => format!("{source}:{}", Utc::now().timestamp_millis()),
                };
                self.add(AttentionItem {
                    id,
                    kind,
                    target: kind.target(),
                    branch,
                    session_id,
                    message,
                    created_at: Utc::now(),
                });
            }
            _ => {}
        }
    }

    fn add(&self, item: AttentionItem) {
        let changed = match self.lock() {
            Ok(mut items) => {
                let id = item.id.clone();
                // Re-adding the same id refreshes the message but keeps one entry.
                items.insert(id, item).is_none()
            }
            Err(err) => {
                crate::error::report(&self.bus, &err);
                return;
            }
        };
        if changed {
            self.publish_updated();
        }
    }

    fn remove(&self, id: &str) {
        if let Ok(mut items) = self.lock() {
            if items.remove(id).is_none() {
                return;
            }
        }
        self.publish_updated();
    }

    /// A session's permission prompts are stale once it is running again.
    fn remove_permissions_of(&self, session_id: &str) {
        let Ok(mut items) = self.lock() else { return };
        let before = items.len();
        items.retain(|_, item| {
            !(item.kind == AttentionKind::PermissionRequest
                && item.session_id.as_deref() == Some(session_id))
        });
        let changed = items.len() != before;
        drop(items);
        if changed {
            self.publish_updated();
        }
    }

    fn publish_updated(&self) {
        let count = self.lock().map(|items| items.len()).unwrap_or(0);
        self.bus.publish(Event::AttentionUpdated { count });
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, AttentionItem>>> {
        self.items.lock().map_err(|_| MaestroError::InvalidData {
            message: "attention queue lock poisoned".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (Arc<AttentionManager>, EventBus) {
        let bus = EventBus::new();
        (Arc::new(AttentionManager::new(bus.clone())), bus)
    }

    #[tokio::test]
    async fn escalation_sessions_stay_out_of_the_queue() {
        let (mgr, _bus) = manager();

        mgr.handle(Event::EscalationStarted {
            asking_session_id: "asking".into(),
            target_session_id: "target".into(),
            escalated_session_id: "escalated".into(),
            question: "why?".into(),
        });

        // Neither its permission prompts nor its failure are the user's problem.
        mgr.handle(Event::SessionPermissionRequest {
            session_id: "escalated".into(),
            request_id: "req-1".into(),
            tool: "Bash".into(),
            args: serde_json::json!({}),
            title: None,
        });
        mgr.handle(Event::SessionStatusChanged {
            session_id: "escalated".into(),
            branch: "impl/x".into(),
            status: SessionStatus::Failed,
        });
        assert!(mgr.list().unwrap().is_empty());

        // A normal session on the same branch is unaffected.
        mgr.handle(Event::SessionStatusChanged {
            session_id: "normal".into(),
            branch: "impl/x".into(),
            status: SessionStatus::Failed,
        });
        assert_eq!(mgr.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn question_dialog_enqueues_and_clears_on_resolution() {
        let (mgr, _bus) = manager();

        mgr.handle(Event::SessionUserDialog {
            session_id: "s1".into(),
            request_id: "dlg-1".into(),
            dialog_kind: "ask_user_question".into(),
            payload: serde_json::json!({ "questions": [] }),
        });
        let items = mgr.list().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, AttentionKind::Question);
        assert_eq!(items[0].target, AttentionTarget::Chat);

        // Answered (or dismissed, or timed out): the queue must not keep nagging.
        mgr.handle(Event::SessionUserDialogResolved {
            session_id: "s1".into(),
            request_id: "dlg-1".into(),
        });
        assert!(mgr.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permission_and_gate_events_enqueue_once() {
        let (mgr, _bus) = manager();

        for _ in 0..2 {
            mgr.handle(Event::SessionPermissionRequest {
                session_id: "s1".into(),
                request_id: "req-1".into(),
                tool: "Bash".into(),
                args: serde_json::json!({}),
                title: Some("Claude wants to run ls".into()),
            });
        }
        assert_eq!(
            mgr.list().unwrap().len(),
            1,
            "same request is not duplicated"
        );

        mgr.handle(Event::GatePending {
            gate_id: "g1".into(),
            session_id: "s1".into(),
            tool: "Bash".into(),
            kind: "git push".into(),
            branch: "impl/T-9".into(),
            params: Vec::new(),
            note: None,
            raw_args: serde_json::json!({}),
        });

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, AttentionKind::Gate, "gates outrank prompts");
        assert_eq!(list[0].target, AttentionTarget::Gate);
    }

    #[tokio::test]
    async fn resolving_the_source_clears_the_item() {
        let (mgr, _bus) = manager();
        mgr.handle(Event::GatePending {
            gate_id: "g1".into(),
            session_id: "s1".into(),
            tool: "Bash".into(),
            kind: "git push".into(),
            branch: "impl/T-9".into(),
            params: Vec::new(),
            note: None,
            raw_args: serde_json::json!({}),
        });
        mgr.handle(Event::GateResolved {
            gate_id: "g1".into(),
            reason: "allowed".into(),
        });
        assert!(mgr.list().unwrap().is_empty());

        // A permission prompt clears when its session resumes streaming.
        mgr.handle(Event::SessionPermissionRequest {
            session_id: "s2".into(),
            request_id: "req-2".into(),
            tool: "Edit".into(),
            args: serde_json::json!({}),
            title: None,
        });
        mgr.handle(Event::SessionStatusChanged {
            session_id: "s2".into(),
            branch: "impl/T-9".into(),
            status: SessionStatus::Streaming,
        });
        assert!(mgr.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failures_and_line_questions_are_queued_and_dismissable() {
        let (mgr, _bus) = manager();
        mgr.handle(Event::SessionStatusChanged {
            session_id: "s3".into(),
            branch: "impl/T-9".into(),
            status: SessionStatus::Failed,
        });
        mgr.handle(Event::AttentionRequired {
            source: "line_question".into(),
            branch: Some("impl/T-9".into()),
            session_id: Some("s4".into()),
            message: "Answer ready for src/lib.rs:3-5".into(),
        });

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, AttentionKind::SessionFailed, "failure first");
        assert_eq!(list[1].target, AttentionTarget::Diff);

        mgr.dismiss(&list[0].id).unwrap();
        assert_eq!(mgr.list().unwrap().len(), 1);
        mgr.dismiss_session("s4").unwrap();
        assert!(mgr.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dismiss_all_clears_everything_but_gates() {
        let (mgr, _bus) = manager();
        mgr.handle(Event::SessionStatusChanged {
            session_id: "s1".into(),
            branch: "impl/a".into(),
            status: SessionStatus::Failed,
        });
        mgr.handle(Event::AttentionRequired {
            source: "line_question".into(),
            branch: Some("impl/a".into()),
            session_id: Some("s2".into()),
            message: "Answer ready".into(),
        });
        mgr.handle(Event::GatePending {
            gate_id: "g1".into(),
            session_id: "s3".into(),
            tool: "Bash".into(),
            kind: "git push".into(),
            branch: "impl/a".into(),
            params: Vec::new(),
            note: None,
            raw_args: serde_json::json!({}),
        });

        assert_eq!(mgr.dismiss_all().unwrap(), 2);
        let left = mgr.list().unwrap();
        assert_eq!(left.len(), 1, "the gate must survive");
        assert_eq!(left[0].kind, AttentionKind::Gate);
        assert_eq!(mgr.dismiss_all().unwrap(), 0, "idempotent on gates only");
    }

    #[tokio::test]
    async fn red_team_findings_point_at_the_notes_tab() {
        let (mgr, _bus) = manager();
        mgr.handle(Event::AttentionRequired {
            source: "red_team_ready".into(),
            branch: Some("redteam/impl-T-9".into()),
            session_id: Some("rt1".into()),
            message: "Red team finished".into(),
        });

        let list = mgr.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, AttentionKind::RedTeamReady);
        // The deliverable is REDTEAM.md, so clicking must land where it renders.
        assert_eq!(list[0].target, AttentionTarget::Notes);
    }

    #[tokio::test]
    async fn changes_publish_attention_updated() {
        let (mgr, bus) = manager();
        let mut rx = bus.subscribe();
        mgr.handle(Event::SessionStatusChanged {
            session_id: "s5".into(),
            branch: "b".into(),
            status: SessionStatus::Failed,
        });
        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "attention.updated");
    }
}
