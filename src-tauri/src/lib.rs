pub mod core;
pub mod error;
pub mod ipc;

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::agent::{SidecarConfig, SidecarEngine};
use crate::core::bus::EventBus;
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

    let worktrees = Arc::new(WorktreeManager::new(
        Arc::new(GitCli),
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

    let state = AppState {
        bus: bus.clone(),
        store,
        worktrees,
        sessions: sessions.clone(),
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
            ipc::delete_session
        ])
        .setup(move |app| {
            ipc::spawn_event_forwarder(app.handle().clone(), bus.clone());
            tauri::async_runtime::spawn(sessions.clone().run_loop(signal_rx));
            tracing::info!("event forwarder and session manager loop started");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
