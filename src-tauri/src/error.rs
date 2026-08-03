//! Typed error hierarchy for the core layer.
//!
//! Agent operations fail routinely (rate limits, network, cancellation) — these are
//! normal states with severity levels, not panics. Every error can be turned into an
//! `error.raised` event and published on the bus so the UI (and future daemon) can react.

use serde::Serialize;

use crate::core::bus::{Event, EventBus};

/// How bad an error is, from the user's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational; expected during normal operation (e.g. cancellation).
    Info,
    /// Something degraded but the operation can continue or be retried.
    Warning,
    /// The operation failed; user action may be required.
    Error,
    /// The application cannot function correctly (e.g. store unavailable).
    Critical,
}

/// What went wrong on the git boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitErrorKind {
    /// The `git` executable could not be launched at all.
    NotInstalled,
    /// The given path is not inside a git repository.
    NotARepository,
    /// git ran but exited non-zero; message carries stderr.
    CommandFailed,
    /// Caller-supplied input (branch name, path) is unusable.
    InvalidInput,
}

/// Core error type. Grows one variant per subsystem as tasks are implemented.
#[derive(Debug, thiserror::Error)]
pub enum MaestroError {
    #[error("database error: {0}")]
    Store(#[from] rusqlite::Error),

    #[error("git: {message}")]
    Git { kind: GitErrorKind, message: String },

    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("configuration error: {message}")]
    Config { message: String },

    #[error("invalid data: {message}")]
    InvalidData { message: String },
}

pub type Result<T> = std::result::Result<T, MaestroError>;

impl MaestroError {
    /// Stable machine-readable code, used in events and logs.
    pub fn code(&self) -> &'static str {
        match self {
            MaestroError::Store(_) => "store",
            MaestroError::Git { .. } => "git",
            MaestroError::Migration(_) => "migration",
            MaestroError::Io(_) => "io",
            MaestroError::Config { .. } => "config",
            MaestroError::InvalidData { .. } => "invalid_data",
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            MaestroError::Store(_) | MaestroError::Migration(_) => Severity::Critical,
            MaestroError::Git { kind, .. } => match kind {
                GitErrorKind::NotInstalled => Severity::Critical,
                GitErrorKind::InvalidInput => Severity::Warning,
                _ => Severity::Error,
            },
            MaestroError::Io(_) => Severity::Error,
            MaestroError::Config { .. } => Severity::Error,
            MaestroError::InvalidData { .. } => Severity::Warning,
        }
    }

    pub fn to_event(&self) -> Event {
        Event::ErrorRaised {
            severity: self.severity(),
            code: self.code().to_string(),
            message: self.to_string(),
        }
    }
}

/// Log an error and publish it as an `error.raised` event.
pub fn report(bus: &EventBus, err: &MaestroError) {
    tracing::error!(code = err.code(), severity = ?err.severity(), "{err}");
    bus.publish(err.to_event());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_mapping() {
        let err = MaestroError::InvalidData {
            message: "bad".into(),
        };
        assert_eq!(err.severity(), Severity::Warning);
        assert_eq!(err.code(), "invalid_data");
    }

    #[test]
    fn error_event_serializes_with_event_name() {
        let err = MaestroError::Config {
            message: "missing value".into(),
        };
        let json = serde_json::to_value(err.to_event()).expect("serialize");
        assert_eq!(json["type"], "error.raised");
        assert_eq!(json["data"]["severity"], "error");
        assert_eq!(json["data"]["code"], "config");
    }
}
