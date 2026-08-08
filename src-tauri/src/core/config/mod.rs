//! App config file (`~/.maestro/config.toml`) — T10.
//!
//! Everything configurable already has a settings key in the store; this module gives
//! those keys a hand-editable home. On startup the file is created with commented
//! defaults if missing, then every value present in it is written into the settings
//! table. That keeps exactly one lookup path at runtime (`Store::get_setting`) while the
//! user edits a file instead of poking SQLite.

use std::path::Path;

use serde::Deserialize;

use crate::core::attention::SETTING_OS_NOTIFICATIONS;
use crate::core::checks::{SETTING_CHECK_AUTO, SETTING_CHECK_COMMAND};
use crate::core::compose::SETTING_COMPOSE_MODEL;
use crate::core::daemon::jira::{
    SETTING_JIRA_BASE_URL, SETTING_JIRA_EMAIL, SETTING_JIRA_JQL, SETTING_JIRA_TOKEN,
};
use crate::core::daemon::{
    SETTING_DAEMON_ACCOUNT, SETTING_DAEMON_ENABLED, SETTING_DAEMON_POLL_MINUTES,
    SETTING_DAEMON_REPO, SETTING_DAEMON_RESEARCH_MODEL, SETTING_DAEMON_USAGE_THRESHOLD,
    SETTING_DAEMON_VERIFY_MODEL,
};
use crate::core::gate::SETTING_GATE_COMMIT;
use crate::core::launcher::SETTING_EDITOR_COMMAND;
use crate::core::questions::SETTING_LINE_QUESTION_TARGET;
use crate::core::session::manager::{SETTING_NOTES_FINALIZE_TIMEOUT, SETTING_SINGLE_WRITER_POLICY};
use crate::core::store::Store;
use crate::core::worktree::{SETTING_BRANCH_TEMPLATE, SETTING_WORKTREE_ROOT};
use crate::error::Result;

/// The file written on first run. Values are commented out so the built-in defaults stay
/// in charge until the user opts in — and so the file documents what can be changed.
const DEFAULT_CONFIG: &str = r#"# MaestroIDE configuration (~/.maestro/config.toml)
#
# Values here are applied at startup. Uncomment a line to override the default.
# Prompt templates are separate files in ~/.maestro/prompts/.

# Branch naming convention for new worktrees.
# branch_naming = "{type}/{task-id}-{slug}"

# Where worktrees are created. Default: beside the repository, in
# <parent>/<repo-name>.worktrees. A configured root gets a per-repository subdirectory.
# worktree_root = "D:/maestro-worktrees"

# What happens when a second write-capable session is started on one worktree:
#   "read_only" (default) — start it in read-only (plan) mode
#   "reject"              — refuse to start it
# single_writer_policy = "read_only"

# Where a line question goes:
#   "active_session" (default) — follow-up to the worktree's live session
#   "fresh_session"            — always a new read-only session
# line_question_target = "active_session"

# Gate `git commit` for approval as well as push and PR creation.
# gate_commit = false

# OS notifications when an agent needs you (also toggleable in the app).
# os_notifications = false

# How long to wait for an implementation session's last turn to write TASK_NOTES.md
# when it is closed. 0 disables the finalize step entirely.
# notes_finalize_timeout_secs = 120

# Editor used by the "Open in editor" button (executable path or name; the worktree
# path is passed as its argument). Default: auto-detect JetBrains Rider.
# editor_command = "C:/Program Files/JetBrains/JetBrains Rider 2025.1/bin/rider64.exe"

# Check command run inside a worktree by the "Run checks" action (build, tests,
# lint — whatever proves the branch is healthy). Empty = feature hidden.
# check_command = "dotnet build"

# Also run the check automatically whenever a session on the branch finishes.
# check_auto = false

# --- GitHub daemon (Этап 3) ---
# Watches assigned issues and PR review comments, spawns read-only research
# sessions. It never commits, never posts to GitHub — human in the loop.

# Master switch (also toggleable from the daemon panel in the app).
# daemon_enabled = false

# Which gh account the daemon acts as. Default: gh's active account.
# The global gh active account is never switched — the token is passed per call.
# daemon_account = "ESievierin"

# Repository to watch as "owner/name". Default: derived from the open
# repository's origin remote.
# daemon_repo = "owner/repo"

# How often to poll GitHub, in minutes.
# daemon_poll_minutes = 5

# Hold the task queue while 5h-window utilization is above this percentage.
# daemon_usage_threshold = 50.0

# Model for issue-research sessions (empty = session default).
# daemon_research_model = "sonnet"

# Model for PR-review / PR-comment-verification sessions (empty = session default).
# daemon_verify_model = "sonnet"

# --- Jira (research flow) ---
# All three must be set for the daemon to poll Jira. The token is an Atlassian
# API token (id.atlassian.com → Security → API tokens), not your password.
# jira_base_url = "https://yourorg.atlassian.net"
# jira_email = "you@yourorg.com"
# jira_token = "..."

# What counts as "my work". Default: open unresolved issues assigned to you.
# jira_jql = "assignee = currentUser() AND resolution = EMPTY ORDER BY updated DESC"

# Model for one-shot generation: commit messages, PR descriptions, reply drafts
# (empty = the Claude CLI's default model).
# compose_model = "sonnet"
"#;

/// Parsed config file. Every field is optional: absent means "leave the current setting".
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub branch_naming: Option<String>,
    pub worktree_root: Option<String>,
    pub single_writer_policy: Option<String>,
    pub line_question_target: Option<String>,
    pub gate_commit: Option<bool>,
    pub os_notifications: Option<bool>,
    pub notes_finalize_timeout_secs: Option<u64>,
    pub editor_command: Option<String>,
    pub check_command: Option<String>,
    pub check_auto: Option<bool>,
    pub daemon_enabled: Option<bool>,
    pub daemon_account: Option<String>,
    pub daemon_repo: Option<String>,
    pub daemon_poll_minutes: Option<u64>,
    pub daemon_usage_threshold: Option<f64>,
    pub daemon_research_model: Option<String>,
    pub daemon_verify_model: Option<String>,
    pub jira_base_url: Option<String>,
    pub jira_email: Option<String>,
    pub jira_token: Option<String>,
    pub jira_jql: Option<String>,
    pub compose_model: Option<String>,
}

impl Config {
    /// Read `path`, creating it with commented defaults when missing. A malformed file is
    /// reported and ignored rather than blocking startup — the app still runs on defaults.
    pub fn load_or_create(path: &Path) -> Self {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    tracing::warn!(error = %err, "could not create config directory");
                    return Self::default();
                }
            }
            match std::fs::write(path, DEFAULT_CONFIG) {
                Ok(()) => tracing::info!(path = %path.display(), "wrote default config"),
                Err(err) => tracing::warn!(error = %err, "could not write default config"),
            }
            return Self::default();
        }

        match std::fs::read_to_string(path) {
            Ok(raw) => match toml::from_str::<Config>(&raw) {
                Ok(config) => config,
                Err(err) => {
                    tracing::error!(path = %path.display(), error = %err, "invalid config.toml; using defaults");
                    Self::default()
                }
            },
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "could not read config.toml");
                Self::default()
            }
        }
    }

    /// Write the values present in the file into the settings table, so the rest of the
    /// core keeps reading settings and knows nothing about the file.
    pub fn apply(&self, store: &dyn Store) -> Result<()> {
        let mut applied = Vec::new();
        for (key, value) in [
            (SETTING_BRANCH_TEMPLATE, self.branch_naming.clone()),
            (SETTING_WORKTREE_ROOT, self.worktree_root.clone()),
            (
                SETTING_SINGLE_WRITER_POLICY,
                self.single_writer_policy.clone(),
            ),
            (
                SETTING_LINE_QUESTION_TARGET,
                self.line_question_target.clone(),
            ),
            (SETTING_GATE_COMMIT, self.gate_commit.map(|b| b.to_string())),
            (
                SETTING_OS_NOTIFICATIONS,
                self.os_notifications.map(|b| b.to_string()),
            ),
            (
                SETTING_NOTES_FINALIZE_TIMEOUT,
                self.notes_finalize_timeout_secs.map(|s| s.to_string()),
            ),
            (SETTING_EDITOR_COMMAND, self.editor_command.clone()),
            (SETTING_CHECK_COMMAND, self.check_command.clone()),
            (SETTING_CHECK_AUTO, self.check_auto.map(|b| b.to_string())),
            (
                SETTING_DAEMON_ENABLED,
                self.daemon_enabled.map(|b| b.to_string()),
            ),
            (SETTING_DAEMON_ACCOUNT, self.daemon_account.clone()),
            (SETTING_DAEMON_REPO, self.daemon_repo.clone()),
            (
                SETTING_DAEMON_POLL_MINUTES,
                self.daemon_poll_minutes.map(|m| m.to_string()),
            ),
            (
                SETTING_DAEMON_USAGE_THRESHOLD,
                self.daemon_usage_threshold.map(|t| t.to_string()),
            ),
            (
                SETTING_DAEMON_RESEARCH_MODEL,
                self.daemon_research_model.clone(),
            ),
            (
                SETTING_DAEMON_VERIFY_MODEL,
                self.daemon_verify_model.clone(),
            ),
            (SETTING_JIRA_BASE_URL, self.jira_base_url.clone()),
            (SETTING_JIRA_EMAIL, self.jira_email.clone()),
            (SETTING_JIRA_TOKEN, self.jira_token.clone()),
            (SETTING_JIRA_JQL, self.jira_jql.clone()),
            (SETTING_COMPOSE_MODEL, self.compose_model.clone()),
        ] {
            if let Some(value) = value {
                store.set_setting(key, &value)?;
                applied.push(key);
            }
        }
        if applied.is_empty() {
            tracing::debug!("config.toml has no overrides");
        } else {
            tracing::info!(keys = ?applied, "config.toml applied to settings");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;

    #[test]
    fn missing_file_is_created_with_commented_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        let config = Config::load_or_create(&path);

        assert!(path.exists(), "the file is written on first run");
        assert!(config.branch_naming.is_none(), "defaults stay in charge");
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.contains("# branch_naming"), "documented but inert");

        // A second run must not clobber a file the user has edited.
        std::fs::write(&path, "gate_commit = true\n").expect("edit");
        let reloaded = Config::load_or_create(&path);
        assert_eq!(reloaded.gate_commit, Some(true));
    }

    #[test]
    fn values_land_in_the_settings_table() {
        let store = SqliteStore::open_in_memory().expect("store");
        let config = Config {
            branch_naming: Some("{type}/{task-id}".into()),
            single_writer_policy: Some("reject".into()),
            line_question_target: None,
            gate_commit: Some(true),
            os_notifications: Some(false),
            notes_finalize_timeout_secs: Some(30),
            worktree_root: Some("D:/wt".into()),
            editor_command: Some("D:/tools/rider64.exe".into()),
            check_command: Some("dotnet build".into()),
            check_auto: Some(true),
            ..Default::default()
        };
        config.apply(&store).expect("apply");

        assert_eq!(
            store
                .get_setting(SETTING_BRANCH_TEMPLATE)
                .unwrap()
                .as_deref(),
            Some("{type}/{task-id}")
        );
        assert_eq!(
            store
                .get_setting(SETTING_SINGLE_WRITER_POLICY)
                .unwrap()
                .as_deref(),
            Some("reject")
        );
        assert_eq!(
            store.get_setting(SETTING_GATE_COMMIT).unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(
            store
                .get_setting(SETTING_OS_NOTIFICATIONS)
                .unwrap()
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            store.get_setting(SETTING_LINE_QUESTION_TARGET).unwrap(),
            None,
            "absent keys are left alone"
        );
    }

    #[test]
    fn malformed_config_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is not = valid = toml").expect("write");
        let config = Config::load_or_create(&path);
        assert!(config.gate_commit.is_none(), "startup is not blocked");
    }
}
