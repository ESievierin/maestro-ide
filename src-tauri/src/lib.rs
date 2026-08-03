pub mod core;
pub mod error;
pub mod ipc;

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::agent::{SidecarConfig, SidecarEngine};
use crate::core::bus::EventBus;
use crate::core::diff::DiffManager;
use crate::core::prompts::PromptManager;
use crate::core::questions::LineQuestionManager;
use crate::core::session::SessionManager;
use crate::core::store::SqliteStore;
use crate::core::worktree::{GitCli, WorktreeManager};
use crate::ipc::AppState;

/// Directory holding all persistent Maestro state (`~/.maestro`).
fn maestro_home() -> PathBuf {
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

    let git: Arc<GitCli> = Arc::new(GitCli);
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
    let sessions = Arc::new(SessionManager::new(store.clone(), bus.clone(), engine));

    // Sessions from a previous app run cannot be reattached (yet) — fail them.
    sessions.fail_stale_sessions("app restart");

    let diffs = Arc::new(DiffManager::new(
        git,
        store.clone(),
        worktrees.clone(),
        bus.clone(),
    ));

    let prompts_dir = maestro_home().join("prompts");
    let prompts = match PromptManager::new(&prompts_dir) {
        Ok(prompts) => Arc::new(prompts),
        Err(err) => {
            tracing::error!(error = %err, path = %prompts_dir.display(), "failed to set up prompts directory");
            std::process::exit(1);
        }
    };
    let questions = Arc::new(LineQuestionManager::new(
        diffs.clone(),
        sessions.clone(),
        worktrees.clone(),
        store.clone(),
        prompts,
        bus.clone(),
    ));

    let state = AppState {
        bus: bus.clone(),
        store,
        worktrees,
        sessions: sessions.clone(),
        diffs: diffs.clone(),
        questions: questions.clone(),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ipc::emit_test_event,
            ipc::list_branches,
            ipc::get_workspace,
            ipc::set_repo,
            ipc::list_worktrees,
            ipc::create_worktree,
            ipc::remove_worktree,
            ipc::spawn_session,
            ipc::send_prompt,
            ipc::interrupt_session,
            ipc::close_session,
            ipc::respond_permission,
            ipc::list_sessions,
            ipc::delete_session,
            ipc::get_diff,
            ipc::refresh_diff,
            ipc::get_file_diff,
            ipc::blame_range,
            ipc::ask_line_question
        ])
        .setup(move |app| {
            ipc::spawn_event_forwarder(app.handle().clone(), bus.clone());
            tauri::async_runtime::spawn(sessions.clone().run_loop(signal_rx));
            tauri::async_runtime::spawn(diffs.clone().run_invalidation_loop(bus.clone()));
            tauri::async_runtime::spawn(questions.clone().run_loop(bus.clone()));
            tracing::info!("event forwarder, session manager, and diff invalidator started");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
