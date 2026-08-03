//! Session entity, state machine, and lifecycle manager.
//!
//! A session is a first-class entity bound to a branch at spawn time and persisted in
//! the store. The manager drives the state machine from sidecar events and publishes
//! every change on the bus.

mod manager;

pub use manager::{SessionManager, SpawnParams};

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
}

impl SessionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionType::Research => "research",
            SessionType::Implementation => "implementation",
            SessionType::ReviewFix => "review_fix",
            SessionType::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "research" => Some(SessionType::Research),
            "implementation" => Some(SessionType::Implementation),
            "review_fix" => Some(SessionType::ReviewFix),
            "manual" => Some(SessionType::Manual),
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
