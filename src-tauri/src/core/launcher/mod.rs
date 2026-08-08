//! Opening a worktree in external tools: the file explorer and the user's editor
//! (Rider first and foremost — reviewing a merge in a familiar IDE is a core part
//! of the workflow this app orchestrates, not an afterthought).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::store::Store;
use crate::error::{MaestroError, Result};

/// Editor launch command. Empty/absent means "auto-detect Rider".
pub const SETTING_EDITOR_COMMAND: &str = "editor_command";

/// Open `path` in the OS file explorer. Spawn-and-forget: explorer.exe is known
/// to return nonzero even on success, so there is no status to meaningfully check.
pub fn open_in_explorer(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    let (program, args): (&str, Vec<String>) =
        ("explorer", vec![path.to_string_lossy().into_owned()]);
    #[cfg(target_os = "macos")]
    let (program, args): (&str, Vec<String>) = ("open", vec![path.to_string_lossy().into_owned()]);
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let (program, args): (&str, Vec<String>) =
        ("xdg-open", vec![path.to_string_lossy().into_owned()]);

    spawn_detached(program, &args)
}

/// Open `path` in the configured editor, auto-detecting Rider when nothing is
/// configured. The `editor_command` setting holds the executable (path or name);
/// the worktree path is passed as its single argument.
pub fn open_in_editor(store: &dyn Store, path: &Path) -> Result<()> {
    let editor = resolve_editor(store)?;
    let (program, args) = editor_invocation(&editor, path);
    spawn_detached(&program, &args)
}

/// The editor executable to use: the `editor_command` setting when set, otherwise
/// the first Rider installation found in the usual places.
fn resolve_editor(store: &dyn Store) -> Result<String> {
    if let Some(configured) = store.get_setting(SETTING_EDITOR_COMMAND)? {
        let trimmed = configured.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    detect_rider().ok_or_else(|| MaestroError::Config {
        message: "no editor found — set `editor_command` in ~/.maestro/config.toml \
                  (e.g. the full path to rider64.exe)"
            .into(),
    })
}

/// Best-effort Rider discovery on Windows: PATH shims first (JetBrains Toolbox
/// puts `rider.cmd` in its scripts directory), then the standard install roots.
fn detect_rider() -> Option<String> {
    // PATH: `where` finds both .exe and .cmd shims.
    if let Ok(output) = Command::new("where").arg("rider").output() {
        if output.status.success() {
            if let Some(first) = String::from_utf8_lossy(&output.stdout).lines().next() {
                let hit = first.trim();
                if !hit.is_empty() {
                    return Some(hit.to_string());
                }
            }
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        // Toolbox's stable shim location, then its per-app install dir.
        roots.push(PathBuf::from(&local).join("JetBrains/Toolbox/scripts/rider.cmd"));
        roots.push(PathBuf::from(&local).join("Programs/Rider/bin/rider64.exe"));
    }
    for root in roots {
        if root.exists() {
            return Some(root.to_string_lossy().into_owned());
        }
    }

    // Classic installer: C:\Program Files\JetBrains\JetBrains Rider <ver>\bin\rider64.exe
    let program_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let jetbrains = PathBuf::from(program_files).join("JetBrains");
    if let Ok(entries) = std::fs::read_dir(&jetbrains) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("JetBrains Rider"))
                    .unwrap_or(false)
            })
            .map(|p| p.join("bin/rider64.exe"))
            .filter(|p| p.exists())
            .collect();
        // Newest version last alphabetically ("JetBrains Rider 2025.1" > "... 2024.3").
        candidates.sort();
        if let Some(best) = candidates.pop() {
            return Some(best.to_string_lossy().into_owned());
        }
    }
    None
}

/// How to invoke `editor` with `path`: batch shims (.cmd/.bat) cannot be spawned
/// directly on Windows — they need `cmd /C`.
fn editor_invocation(editor: &str, path: &Path) -> (String, Vec<String>) {
    let lower = editor.to_ascii_lowercase();
    let path_arg = path.to_string_lossy().into_owned();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), editor.to_string(), path_arg],
        )
    } else {
        (editor.to_string(), vec![path_arg])
    }
}

fn spawn_detached(program: &str, args: &[String]) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console flash for cmd-wrapped shims.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn().map_err(|err| MaestroError::Config {
        message: format!("failed to launch `{program}`: {err}"),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;

    #[test]
    fn configured_editor_wins_over_detection() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .set_setting(SETTING_EDITOR_COMMAND, "D:/tools/myeditor.exe")
            .expect("set");
        assert_eq!(
            resolve_editor(&store).expect("resolve"),
            "D:/tools/myeditor.exe"
        );
    }

    #[test]
    fn blank_setting_falls_through_to_detection() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .set_setting(SETTING_EDITOR_COMMAND, "   ")
            .expect("set");
        // Detection may or may not find Rider on the machine running the tests;
        // the contract under test is only that a blank setting is not returned.
        match resolve_editor(&store) {
            Ok(editor) => assert!(!editor.trim().is_empty()),
            Err(err) => assert!(err.to_string().contains("editor_command"), "{err}"),
        }
    }

    #[test]
    fn cmd_shims_are_wrapped_and_executables_are_not() {
        let path = Path::new("C:/work/repo.worktrees/impl-T-1-x");
        let (program, args) =
            editor_invocation("C:/Users/u/JetBrains/Toolbox/scripts/rider.cmd", path);
        assert_eq!(program, "cmd");
        assert_eq!(args[0], "/C");
        assert!(args[1].ends_with("rider.cmd"));
        assert!(args[2].contains("impl-T-1-x"));

        let (program, args) = editor_invocation("C:/Rider/bin/rider64.exe", path);
        assert_eq!(program, "C:/Rider/bin/rider64.exe");
        assert_eq!(args.len(), 1);
    }
}
