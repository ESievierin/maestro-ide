pub mod core;
pub mod error;
pub mod ipc;

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::agent::{SidecarConfig, SidecarEngine};
use crate::core::attention::AttentionManager;
use crate::core::bus::EventBus;
use crate::core::checks::ChecksManager;
use crate::core::compose::ComposeManager;
use crate::core::config::Config;
use crate::core::daemon::{DaemonManager, GhCli, JiraCli, RealDaemonExec};
use crate::core::diff::DiffManager;
use crate::core::escalation::EscalationManager;
use crate::core::gate::{self, GateManager};
use crate::core::impact::ImpactManager;
use crate::core::notes::NotesManager;
use crate::core::pr::PrManager;
use crate::core::prompts::PromptManager;
use crate::core::questions::LineQuestionManager;
use crate::core::session::SessionManager;
use crate::core::store::{SqliteStore, Store};
use crate::core::telemetry::TelemetryManager;
use crate::core::worktree::{GitCli, WorktreeManager};
use crate::ipc::AppState;

/// Directory holding all persistent Maestro state (`~/.maestro`, or `$MAESTRO_HOME` when
/// set — an isolated profile for a second instance without touching the real one).
pub fn maestro_home() -> PathBuf {
    if let Ok(custom) = std::env::var("MAESTRO_HOME") {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .map(|home| home.join(".maestro"))
        .unwrap_or_else(|| PathBuf::from(".maestro"))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,maestro_lib=debug")),
        )
        .init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let bus = EventBus::new();

    let db_path = maestro_home().join("maestro.db");
    let store = match SqliteStore::open(&db_path) {
        Ok(store) => Arc::new(store),
        Err(err) => {
            // Without the store the app cannot function; fail loudly at startup.
            tracing::error!(error = %err, path = %db_path.display(), "failed to open store");
            std::process::exit(1);
        }
    };
    tracing::info!(path = %db_path.display(), "store ready");

    // The config file seeds the settings table, so everything downstream keeps
    // reading settings and knows nothing about the file.
    let config_path = maestro_home().join("config.toml");
    if let Err(err) = Config::load_or_create(&config_path).apply(store.as_ref()) {
        tracing::warn!(error = %err, "could not apply config.toml");
    }

    let git: Arc<GitCli> = Arc::new(GitCli::new());
    let worktrees = Arc::new(WorktreeManager::new(
        git.clone(),
        store.clone(),
        bus.clone(),
    ));
    match worktrees.load_persisted_repo() {
        Ok(Some(path)) => tracing::info!(repo = %path.display(), "repository restored"),
        Ok(None) => tracing::info!("no repository selected yet"),
        Err(err) => tracing::warn!(error = %err, "failed to restore repository"),
    }

    // Agent engine: supervised Node sidecar. Signals flow supervisor → manager loop.
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel();
    let engine = Arc::new(SidecarEngine::new(SidecarConfig::resolve(), signal_tx));
    // The escalation manager answers tool calls directly, so it needs the engine too.
    let engine_for_escalation: Arc<dyn core::agent::AgentEngine> = engine.clone();

    // Gate registry: git push / gh pr create (+ git commit behind the
    // `gate_commit` setting) pause for explicit approval before executing.
    let gates = Arc::new(GateManager::new(
        gate::build_registry(store.as_ref()),
        engine.clone(),
        bus.clone(),
    ));
    let diffs = Arc::new(DiffManager::new(
        git.clone(),
        store.clone(),
        worktrees.clone(),
        bus.clone(),
    ));
    let impact = Arc::new(ImpactManager::new(
        git,
        worktrees.clone(),
        diffs.clone(),
        store.clone(),
    ));

    let prompts_dir = maestro_home().join("prompts");
    let prompts = match PromptManager::new(&prompts_dir) {
        Ok(prompts) => Arc::new(prompts),
        Err(err) => {
            tracing::error!(error = %err, path = %prompts_dir.display(), "failed to set up prompts directory");
            std::process::exit(1);
        }
    };

    // TASK_NOTES.md: the record a task leaves for the next agent. The session manager
    // needs it (and the templates) to ask a closing implementation session for its notes.
    let notes = Arc::new(NotesManager::new(worktrees.clone(), bus.clone()));
    let telemetry = Arc::new(TelemetryManager::new(maestro_home().join("telemetry")));
    // Retention housekeeping — config.toml was applied above, so the setting is fresh.
    let retention_days = store
        .get_setting(core::telemetry::SETTING_TELEMETRY_RETENTION_DAYS)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    telemetry.sweep(retention_days);
    let sessions = Arc::new(
        SessionManager::with_gates(store.clone(), bus.clone(), engine, Some(gates.clone()))
            .with_notes(notes.clone(), prompts.clone())
            .with_telemetry(telemetry),
    );

    // The attention queue must witness the stale-session failures below, but its
    // consumer task only spawns in setup() — subscribe now so the events buffer
    // in the channel instead of racing the spawn (and usually losing).
    let attention_rx = bus.subscribe();
    // Sessions from a previous app run cannot be reattached (yet) — fail them.
    sessions.fail_stale_sessions("app restart");
    let questions = Arc::new(
        LineQuestionManager::new(
            diffs.clone(),
            sessions.clone(),
            worktrees.clone(),
            store.clone(),
            prompts.clone(),
            bus.clone(),
        )
        .with_notes(notes.clone()),
    );

    // `ask_original_agent`: a review session can ask the implementing agent why. The two
    // managers reference each other, so the escalation side holds weak references.
    let escalations = Arc::new(EscalationManager::new(
        engine_for_escalation,
        store.clone(),
        sessions.clone(),
        worktrees.clone(),
        bus.clone(),
    ));
    escalations.attach();
    sessions.set_escalation_handler(Arc::downgrade(&escalations)
        as std::sync::Weak<dyn core::session::manager::EscalationHandler>);

    let attention = Arc::new(AttentionManager::new(bus.clone()));
    let checks = Arc::new(ChecksManager::new(
        store.clone(),
        worktrees.clone(),
        bus.clone(),
    ));

    // GitHub/Jira daemon: off by default; polls as the configured account and
    // spawns read-only research sessions. Never posts, never commits.
    let daemon = Arc::new(DaemonManager::new(
        store.clone(),
        Arc::new(GhCli),
        Arc::new(JiraCli),
        Arc::new(RealDaemonExec {
            worktrees: worktrees.clone(),
            sessions: sessions.clone(),
        }),
        bus.clone(),
    ));

    // Prompt rendering (commit messages, PR text) + user-initiated PR actions.
    // Generation itself runs through a real session on the frontend, not a
    // one-shot subprocess — this only gathers git context and renders the
    // editable templates.
    let compose = Arc::new(ComposeManager::new(
        store.clone(),
        worktrees.clone(),
        prompts.clone(),
    ));
    let prs = Arc::new(PrManager::new(
        store.clone(),
        Arc::new(GhCli),
        worktrees.clone(),
        bus.clone(),
    ));

    let redteam = Arc::new(core::redteam::RedTeamManager::new(
        worktrees.clone(),
        sessions.clone(),
        diffs.clone(),
        notes.clone(),
        prompts.clone(),
        store.clone(),
        impact.clone(),
        bus.clone(),
    ));

    let state = AppState {
        bus: bus.clone(),
        store,
        worktrees,
        sessions: sessions.clone(),
        diffs: diffs.clone(),
        impact,
        gates,
        questions: questions.clone(),
        prompts,
        notes,
        attention: attention.clone(),
        checks: checks.clone(),
        daemon: daemon.clone(),
        compose,
        prs,
        redteam: redteam.clone(),
    };

    let sessions_for_shutdown = sessions.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ipc::emit_test_event,
            ipc::list_branches,
            ipc::get_workspace,
            ipc::set_repo,
            ipc::list_worktrees,
            ipc::create_worktree,
            ipc::remove_worktree,
            ipc::set_worktree_pinned,
            ipc::merge_worktree,
            ipc::sync_worktree,
            ipc::commit_worktree,
            ipc::set_line_ending,
            ipc::open_worktree,
            ipc::take_snapshot,
            ipc::list_snapshots,
            ipc::restore_snapshot,
            ipc::drop_snapshot,
            ipc::get_check_command,
            ipc::run_check,
            ipc::get_check,
            ipc::push_worktree,
            ipc::branch_log,
            ipc::spawn_session,
            ipc::ensure_main_session,
            ipc::start_red_team,
            ipc::send_red_team_findings,
            ipc::send_prompt,
            ipc::interrupt_session,
            ipc::close_session,
            ipc::respond_permission,
            ipc::list_sessions,
            ipc::delete_session,
            ipc::save_session_transcript,
            ipc::get_session_transcript,
            ipc::get_diff,
            ipc::analyze_impact,
            ipc::refresh_diff,
            ipc::get_file_diff,
            ipc::blame_range,
            ipc::list_pending_gates,
            ipc::respond_gate,
            ipc::ask_line_question,
            ipc::list_prompts,
            ipc::save_prompt,
            ipc::reset_prompt,
            ipc::delete_prompt,
            ipc::export_settings_bundle,
            ipc::import_settings_bundle,
            ipc::write_text_file,
            ipc::run_health_check,
            ipc::list_attention,
            ipc::dismiss_attention,
            ipc::dismiss_all_attention,
            ipc::refresh_models,
            ipc::set_session_model,
            ipc::set_session_effort,
            ipc::set_session_permission_mode,
            ipc::set_session_thinking,
            ipc::mcp_server_action,
            ipc::get_notes,
            ipc::refresh_notes,
            ipc::respond_user_dialog,
            ipc::daemon_status,
            ipc::list_daemon_tasks,
            ipc::daemon_poll_now,
            ipc::set_daemon_enabled,
            ipc::set_daemon_account,
            ipc::set_daemon_watched_accounts,
            ipc::set_daemon_skip_labels,
            ipc::get_mock_mode,
            ipc::get_maestro_home,
            ipc::preview_prompt,
            ipc::get_red_team_auto,
            ipc::set_red_team_auto,
            ipc::get_telemetry_enabled,
            ipc::set_telemetry_enabled,
            ipc::get_telemetry_retention_days,
            ipc::set_telemetry_retention_days,
            ipc::get_os_notifications_enabled,
            ipc::set_os_notifications_enabled,
            ipc::get_notification_digest_enabled,
            ipc::set_notification_digest_enabled,
            ipc::get_single_writer_policy,
            ipc::set_single_writer_policy,
            ipc::get_branch_naming,
            ipc::set_branch_naming,
            ipc::get_notes_finalize_timeout,
            ipc::set_notes_finalize_timeout,
            ipc::search_sessions,
            ipc::list_session_presets,
            ipc::save_session_preset,
            ipc::delete_session_preset,
            ipc::get_usage_summary,
            ipc::get_usage_by_branch,
            ipc::dismiss_daemon_task,
            ipc::render_review_reply_style,
            ipc::render_review_workflow_gate,
            ipc::render_review_guide_prompt,
            ipc::render_commit_prompt,
            ipc::render_pr_prompt,
            ipc::create_pr,
            ipc::list_pr_comments,
            ipc::reply_pr_comments,
            ipc::post_review_comments,
            ipc::open_url
        ])
        .setup(move |app| {
            ipc::spawn_event_forwarder(app.handle().clone(), bus.clone());
            tauri::async_runtime::spawn(sessions.clone().run_loop(signal_rx));
            tauri::async_runtime::spawn(diffs.clone().run_invalidation_loop(bus.clone()));
            tauri::async_runtime::spawn(questions.clone().run_loop(bus.clone()));
            tauri::async_runtime::spawn(attention.clone().run_with(attention_rx));
            tauri::async_runtime::spawn(escalations.clone().run_loop(bus.clone()));
            tauri::async_runtime::spawn(checks.clone().run_auto_loop(bus.clone()));
            tauri::async_runtime::spawn(redteam.clone().run_auto_loop(bus.clone()));
            tauri::async_runtime::spawn(daemon.clone().run_loop(bus.clone()));
            tracing::info!("event forwarder, session manager, and diff invalidator started");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_app_handle, event| {
            // A clean exit means we know exactly what happened to each session —
            // unlike the blanket "failed" sweep at next startup, which only runs
            // because it has no better information at that point.
            if let tauri::RunEvent::Exit = event {
                sessions_for_shutdown.shutdown_sessions();
            }
        });
}
