//! Session entity, state machine, and lifecycle manager.
//!
//! A session is a first-class entity bound to a branch at spawn time and persisted in
//! the store. The manager drives the state machine from sidecar events and publishes
//! every change on the bus.

pub mod manager;

pub use manager::{SessionManager, SpawnParams, REVIEW_TOOLS_PROFILE};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of work a session performs. Extensible: new variants are additive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Research,
    Implementation,
    ReviewFix,
    Manual,
    /// A short read-only session that answers another session's `ask_original_agent`
    /// question by resuming the original implementation context. Never a writer, never in
    /// the attention queue: nobody waits on it but the asking agent.
    Escalation,
    /// The one persistent, unclosable session pinned to a worktree — created eagerly
    /// alongside it. PR review, reply drafting, and commit/PR-description generation
    /// all resume this session instead of spawning a throwaway one, so the answer
    /// reflects a continuous conversation rather than a cold read every time.
    Main,
    /// Adversarial QA in a child worktree branched off the branch under attack:
    /// hunts edge cases and race conditions, proves each with a failing test,
    /// writes REDTEAM.md. Never touches production code.
    RedTeam,
}

impl SessionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionType::Research => "research",
            SessionType::Escalation => "escalation",
            SessionType::Implementation => "implementation",
            SessionType::ReviewFix => "review_fix",
            SessionType::Manual => "manual",
            SessionType::Main => "main",
            SessionType::RedTeam => "red_team",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "research" => Some(SessionType::Research),
            "implementation" => Some(SessionType::Implementation),
            "review_fix" => Some(SessionType::ReviewFix),
            "escalation" => Some(SessionType::Escalation),
            "manual" => Some(SessionType::Manual),
            "main" => Some(SessionType::Main),
            "red_team" => Some(SessionType::RedTeam),
            _ => None,
        }
    }
}

/// Session lifecycle: `spawning → streaming → awaiting_input → done | failed | cancelled`.
/// Transition validation is enforced by the state machine (T3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning,
    Streaming,
    AwaitingInput,
    Done,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Spawning => "spawning",
            SessionStatus::Streaming => "streaming",
            SessionStatus::AwaitingInput => "awaiting_input",
            SessionStatus::Done => "done",
            SessionStatus::Failed => "failed",
            SessionStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "spawning" => Some(SessionStatus::Spawning),
            "streaming" => Some(SessionStatus::Streaming),
            "awaiting_input" => Some(SessionStatus::AwaitingInput),
            "done" => Some(SessionStatus::Done),
            "failed" => Some(SessionStatus::Failed),
            "cancelled" => Some(SessionStatus::Cancelled),
            _ => None,
        }
    }

    /// Terminal states cannot transition further.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionStatus::Done | SessionStatus::Failed | SessionStatus::Cancelled
        )
    }

    /// State machine: `spawning → streaming ⇄ awaiting_input → done | failed | cancelled`.
    /// Terminal states never transition; self-transitions are rejected.
    pub fn can_transition_to(&self, next: SessionStatus) -> bool {
        use SessionStatus::*;
        if *self == next {
            return false;
        }
        match self {
            Spawning => matches!(next, Streaming | AwaitingInput | Failed | Cancelled),
            Streaming => matches!(next, AwaitingInput | Done | Failed | Cancelled),
            AwaitingInput => matches!(next, Streaming | Done | Failed | Cancelled),
            Done | Failed | Cancelled => false,
        }
    }
}

/// Permission mode that runs without write access (SDK plan mode). Sessions in this
/// mode do not count as writers for the single-writer rule.
pub const READ_ONLY_MODE: &str = "plan";

/// Does this permission mode grant write access to the worktree?
pub fn is_writer_mode(permission_mode: Option<&str>) -> bool {
    permission_mode != Some(READ_ONLY_MODE)
}

/// Thinking options offered in the UI. `default` leaves the CLI alone (adaptive, which in
/// practice often produces no thinking at all); the budgets force it on.
pub const THINKING_OPTIONS: &[&str] = &["default", "off", "4000", "16000", "32000"];

pub fn is_known_thinking(thinking: &str) -> bool {
    thinking.is_empty() || THINKING_OPTIONS.contains(&thinking)
}

/// Reasoning-effort levels the CLI accepts. An empty string clears the override.
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Permission modes the CLI accepts. `bypassPermissions` and `dontAsk` are absent on
/// purpose: with them the SDK never calls `canUseTool`, which is what the commit/push
/// gate hangs on, so they would silently disarm it.
pub const PERMISSION_MODES: &[&str] = &["default", "acceptEdits", "auto", "plan"];

pub fn is_known_effort(effort: &str) -> bool {
    effort.is_empty() || EFFORT_LEVELS.contains(&effort)
}

pub fn is_known_permission_mode(mode: &str) -> bool {
    PERMISSION_MODES.contains(&mode)
}

/// Persisted session row. `branch` is the foreign key linking worktree ↔ task ↔ PR.
/// `sdk_session_id` is the Claude Agent SDK session id, persisted for resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub branch: String,
    pub session_type: SessionType,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Effective permission mode (may have been downgraded by the single-writer rule).
    pub permission_mode: Option<String>,
    /// Thinking budget: `None`/`default` = CLI default, `off`, or a token count.
    pub thinking: Option<String>,
    /// Extra tool profile this session ran with (`review`), persisted for audit and resume.
    pub tools_profile: Option<String>,
    pub sdk_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    /// Create a new session bound to a branch, in the initial `spawning` state.
    pub fn new(
        branch: impl Into<String>,
        session_type: SessionType,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            branch: branch.into(),
            session_type,
            status: SessionStatus::Spawning,
            model,
            effort,
            permission_mode,
            thinking: None,
            tools_profile: None,
            sdk_session_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_writer(&self) -> bool {
        is_writer_mode(self.permission_mode.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_strings() {
        for status in [
            SessionStatus::Spawning,
            SessionStatus::Streaming,
            SessionStatus::AwaitingInput,
            SessionStatus::Done,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
        ] {
            assert_eq!(SessionStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(SessionStatus::parse("bogus"), None);
    }

    #[test]
    fn type_round_trips_through_strings() {
        for ty in [
            SessionType::Research,
            SessionType::Implementation,
            SessionType::ReviewFix,
            SessionType::Manual,
            SessionType::Main,
            SessionType::RedTeam,
        ] {
            assert_eq!(SessionType::parse(ty.as_str()), Some(ty));
        }
    }

    #[test]
    fn terminal_states() {
        assert!(SessionStatus::Done.is_terminal());
        assert!(SessionStatus::Failed.is_terminal());
        assert!(SessionStatus::Cancelled.is_terminal());
        assert!(!SessionStatus::Streaming.is_terminal());
    }

    #[test]
    fn state_machine_transitions() {
        use SessionStatus::*;
        // The happy path.
        assert!(Spawning.can_transition_to(Streaming));
        assert!(Streaming.can_transition_to(AwaitingInput));
        assert!(AwaitingInput.can_transition_to(Streaming));
        assert!(AwaitingInput.can_transition_to(Done));
        // Failure/cancellation from any live state.
        for from in [Spawning, Streaming, AwaitingInput] {
            assert!(from.can_transition_to(Failed));
            assert!(from.can_transition_to(Cancelled));
        }
        // Illegal moves.
        assert!(!Spawning.can_transition_to(Done), "must stream first");
        assert!(!Done.can_transition_to(Streaming), "terminal is terminal");
        assert!(!Failed.can_transition_to(Spawning));
        assert!(!Streaming.can_transition_to(Streaming), "no self-loops");
    }
}
