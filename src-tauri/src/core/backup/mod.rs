//! Export/import bundle: the portable subset of settings, plus every prompt
//! template, as one JSON blob a user can carry between machines or keep as a
//! backup. Deliberately excludes anything machine-specific (`repo_path`,
//! `worktree_root`, `editor_command`, which gh account is active) or secret
//! (the Jira token) — see [`EXPORTABLE_SETTINGS`]. `import` re-checks every
//! incoming key against that same allowlist rather than trusting the file,
//! so a hand-edited or foreign bundle can never smuggle in a setting outside
//! the exported set.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::attention::{SETTING_NOTIFICATION_DIGEST, SETTING_OS_NOTIFICATIONS};
use crate::core::checks::{SETTING_CHECK_AUTO, SETTING_CHECK_COMMAND};
use crate::core::config::{
    SETTING_IMPACT_INCLUDE_REFERENCES, SETTING_RED_TEAM_AUTO, SETTING_RED_TEAM_EFFORT,
    SETTING_RED_TEAM_MODEL,
};
use crate::core::daemon::{
    SETTING_DAEMON_ENABLED, SETTING_DAEMON_POLL_MINUTES, SETTING_DAEMON_RESEARCH_EFFORT,
    SETTING_DAEMON_RESEARCH_MODEL, SETTING_DAEMON_SKIP_LABELS, SETTING_DAEMON_USAGE_THRESHOLD,
    SETTING_DAEMON_VERIFY_EFFORT, SETTING_DAEMON_VERIFY_MODEL,
};
use crate::core::escalation::SETTING_ESCALATION_TIMEOUT;
use crate::core::gate::SETTING_GATE_COMMIT;
use crate::core::prompts::PromptManager;
use crate::core::questions::SETTING_LINE_QUESTION_TARGET;
use crate::core::session::manager::{SETTING_NOTES_FINALIZE_TIMEOUT, SETTING_SINGLE_WRITER_POLICY};
use crate::core::store::Store;
use crate::core::telemetry::{SETTING_TELEMETRY_ENABLED, SETTING_TELEMETRY_RETENTION_DAYS};
use crate::core::worktree::SETTING_BRANCH_TEMPLATE;
use crate::error::{MaestroError, Result};

/// Bumped only if the bundle shape changes in a way `import` must branch on.
pub const BUNDLE_VERSION: u32 = 1;

/// Behavior preferences worth carrying to another machine. Not local paths,
/// not secrets, not a machine's own account selection.
pub const EXPORTABLE_SETTINGS: &[&str] = &[
    SETTING_TELEMETRY_ENABLED,
    SETTING_TELEMETRY_RETENTION_DAYS,
    SETTING_OS_NOTIFICATIONS,
    SETTING_NOTIFICATION_DIGEST,
    SETTING_SINGLE_WRITER_POLICY,
    SETTING_NOTES_FINALIZE_TIMEOUT,
    SETTING_GATE_COMMIT,
    SETTING_CHECK_COMMAND,
    SETTING_CHECK_AUTO,
    SETTING_LINE_QUESTION_TARGET,
    SETTING_ESCALATION_TIMEOUT,
    SETTING_BRANCH_TEMPLATE,
    SETTING_DAEMON_ENABLED,
    SETTING_DAEMON_POLL_MINUTES,
    SETTING_DAEMON_USAGE_THRESHOLD,
    SETTING_DAEMON_RESEARCH_MODEL,
    SETTING_DAEMON_RESEARCH_EFFORT,
    SETTING_DAEMON_VERIFY_MODEL,
    SETTING_DAEMON_VERIFY_EFFORT,
    SETTING_DAEMON_SKIP_LABELS,
    SETTING_RED_TEAM_MODEL,
    SETTING_RED_TEAM_EFFORT,
    SETTING_RED_TEAM_AUTO,
    SETTING_IMPACT_INCLUDE_REFERENCES,
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptEntry {
    pub name: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsBundle {
    pub version: u32,
    pub exported_at: DateTime<Utc>,
    pub settings: BTreeMap<String, String>,
    pub prompts: Vec<PromptEntry>,
}

/// What actually happened on import — the frontend shows this instead of a
/// silent success, since an import overwrites existing customizations.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ImportSummary {
    pub settings_applied: usize,
    pub prompts_written: usize,
    /// Keys the bundle carried that this build does not (or no longer)
    /// recognize as exportable — skipped, not applied, not an error.
    pub settings_skipped: Vec<String>,
}

/// Snapshot every exportable setting that has a value, plus the prompt
/// templates the user actually owns: edited defaults and custom templates.
/// Unedited defaults are deliberately left out — they reinstall themselves on
/// any machine, and carrying them would pin an old bundle's defaults over a
/// newer app's (and knock those files out of the automatic update flow).
pub fn export(store: &dyn Store, prompts: &PromptManager) -> Result<SettingsBundle> {
    let mut settings = BTreeMap::new();
    for key in EXPORTABLE_SETTINGS {
        if let Some(value) = store.get_setting(key)? {
            settings.insert((*key).to_string(), value);
        }
    }
    let prompt_entries = prompts
        .list()?
        .into_iter()
        .filter(|p| p.modified || !p.has_default)
        .map(|p| PromptEntry {
            name: p.name,
            content: p.content,
        })
        .collect();
    Ok(SettingsBundle {
        version: BUNDLE_VERSION,
        exported_at: Utc::now(),
        settings,
        prompts: prompt_entries,
    })
}

/// Apply a bundle: every setting whose key is still in the allowlist is
/// written as-is (last import wins, same as any other settings write); every
/// prompt entry overwrites its file, same as saving it by hand in the editor.
pub fn import(
    store: &dyn Store,
    prompts: &PromptManager,
    bundle: &SettingsBundle,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();
    for (key, value) in &bundle.settings {
        if EXPORTABLE_SETTINGS.contains(&key.as_str()) {
            store.set_setting(key, value)?;
            summary.settings_applied += 1;
        } else {
            summary.settings_skipped.push(key.clone());
        }
    }
    for entry in &bundle.prompts {
        prompts.save(&entry.name, &entry.content)?;
        summary.prompts_written += 1;
    }
    Ok(summary)
}

/// Parse a bundle from its JSON text — a hand-edited or foreign file is a
/// normal, reportable failure, not a panic.
pub fn parse(json: &str) -> Result<SettingsBundle> {
    serde_json::from_str(json).map_err(|err| MaestroError::InvalidData {
        message: format!("not a valid settings bundle: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().expect("open in-memory store")
    }

    #[test]
    fn export_then_import_round_trips_settings_and_prompts() {
        let src = store();
        src.set_setting(SETTING_TELEMETRY_ENABLED, "false").unwrap();
        src.set_setting(SETTING_DAEMON_POLL_MINUTES, "10").unwrap();
        // Not in the allowlist — must never appear in the export.
        src.set_setting("jira_token", "super-secret").unwrap();
        src.set_setting("repo_path", "/Users/alice/repo").unwrap();

        let prompts_dir = tempfile::tempdir().unwrap();
        let prompts = PromptManager::new(prompts_dir.path()).unwrap();
        prompts
            .save("commit-message", "Custom: {{branch}}")
            .unwrap();

        let bundle = export(&src, &prompts).unwrap();
        assert_eq!(
            bundle.settings.get(SETTING_TELEMETRY_ENABLED).unwrap(),
            "false"
        );
        assert_eq!(
            bundle.settings.get(SETTING_DAEMON_POLL_MINUTES).unwrap(),
            "10"
        );
        assert!(
            !bundle.settings.contains_key("jira_token"),
            "a secret must never end up in the exported bundle"
        );
        assert!(!bundle.settings.contains_key("repo_path"));
        assert!(bundle
            .prompts
            .iter()
            .any(|p| p.name == "commit-message" && p.content == "Custom: {{branch}}"));
        assert!(
            !bundle.prompts.iter().any(|p| p.name == "line-question"),
            "unedited defaults stay out of the bundle — they reinstall themselves \
             and must keep receiving shipped updates on the destination"
        );

        let dst = store();
        let dst_prompts_dir = tempfile::tempdir().unwrap();
        let dst_prompts = PromptManager::new(dst_prompts_dir.path()).unwrap();
        let summary = import(&dst, &dst_prompts, &bundle).unwrap();

        assert_eq!(summary.settings_applied, 2);
        assert!(summary.settings_skipped.is_empty());
        assert_eq!(
            dst.get_setting(SETTING_TELEMETRY_ENABLED).unwrap(),
            Some("false".into())
        );
        assert_eq!(
            dst.get_setting(SETTING_DAEMON_POLL_MINUTES).unwrap(),
            Some("10".into())
        );
        assert_eq!(
            dst_prompts.read("commit-message").unwrap().content,
            "Custom: {{branch}}"
        );
    }

    #[test]
    fn import_never_writes_a_setting_outside_the_allowlist() {
        let dst = store();
        let prompts_dir = tempfile::tempdir().unwrap();
        let prompts = PromptManager::new(prompts_dir.path()).unwrap();

        let mut settings = BTreeMap::new();
        settings.insert("jira_token".to_string(), "smuggled-secret".to_string());
        settings.insert("repo_path".to_string(), "/etc/passwd".to_string());
        settings.insert(SETTING_TELEMETRY_ENABLED.to_string(), "false".to_string());
        let bundle = SettingsBundle {
            version: BUNDLE_VERSION,
            exported_at: Utc::now(),
            settings,
            prompts: Vec::new(),
        };

        let summary = import(&dst, &prompts, &bundle).unwrap();
        assert_eq!(summary.settings_applied, 1);
        assert_eq!(summary.settings_skipped.len(), 2);
        assert!(dst.get_setting("jira_token").unwrap().is_none());
        assert!(dst.get_setting("repo_path").unwrap().is_none());
    }

    #[test]
    fn parse_reports_a_readable_error_for_garbage_input() {
        let err = parse("not json at all").unwrap_err();
        assert!(err.to_string().contains("not a valid settings bundle"));
    }

    #[test]
    fn parse_round_trips_through_serde_json() {
        let src = store();
        src.set_setting(SETTING_TELEMETRY_ENABLED, "true").unwrap();
        let prompts_dir = tempfile::tempdir().unwrap();
        let prompts = PromptManager::new(prompts_dir.path()).unwrap();
        let bundle = export(&src, &prompts).unwrap();

        let json = serde_json::to_string(&bundle).unwrap();
        let parsed = parse(&json).unwrap();
        assert_eq!(parsed, bundle);
    }
}
