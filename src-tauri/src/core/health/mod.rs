//! Setup diagnostics: a read-only snapshot of whether the environment this
//! app depends on is actually working. Nothing here calls a network API or
//! changes anything — useful for getting set up on a new machine, especially
//! right after carrying settings over via `core::backup`'s export/import.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::core::daemon::jira::{SETTING_JIRA_BASE_URL, SETTING_JIRA_EMAIL, SETTING_JIRA_TOKEN};
use crate::core::daemon::GhProvider;
use crate::core::launcher;
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;

#[derive(Clone, Debug, Serialize)]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthReport {
    pub checks: Vec<HealthCheck>,
}

/// Run every check. Each one is independent and never panics the others —
/// one broken tool should not hide the state of the rest.
pub fn run(
    store: &dyn Store,
    gh: &dyn GhProvider,
    worktrees: &WorktreeManager,
    config_path: &Path,
) -> HealthReport {
    HealthReport {
        checks: vec![
            check_git(),
            check_gh(gh),
            check_editor(store),
            check_jira(store),
            check_repo(worktrees),
            check_config(config_path),
        ],
    }
}

/// A malformed config.toml is otherwise applied as "no overrides" with only a
/// log line to show for it — the one silent failure mode a user actually hits
/// after hand-editing the file.
fn check_config(path: &Path) -> HealthCheck {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return HealthCheck {
                name: "config.toml".into(),
                ok: true,
                detail: "not created yet — defaults in effect".into(),
            };
        }
        Err(err) => {
            return HealthCheck {
                name: "config.toml".into(),
                ok: false,
                detail: format!("unreadable: {err}"),
            };
        }
    };
    match toml::from_str::<crate::core::config::Config>(&raw) {
        Ok(_) => HealthCheck {
            name: "config.toml".into(),
            ok: true,
            detail: path.to_string_lossy().into_owned(),
        },
        Err(err) => HealthCheck {
            name: "config.toml".into(),
            ok: false,
            // toml's message already names the line/column; first line is enough.
            detail: format!(
                "ignored (defaults in effect): {}",
                err.to_string().lines().next().unwrap_or("parse error")
            ),
        },
    }
}

fn check_git() -> HealthCheck {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => HealthCheck {
            name: "git".into(),
            ok: true,
            detail: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => HealthCheck {
            name: "git".into(),
            ok: false,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(err) => HealthCheck {
            name: "git".into(),
            ok: false,
            detail: format!("not found on PATH: {err}"),
        },
    }
}

fn check_gh(gh: &dyn GhProvider) -> HealthCheck {
    match gh.accounts() {
        Ok(accounts) if !accounts.is_empty() => HealthCheck {
            name: "gh".into(),
            ok: true,
            detail: format!(
                "{} account{} authenticated: {}",
                accounts.len(),
                if accounts.len() == 1 { "" } else { "s" },
                accounts
                    .iter()
                    .map(|a| a.login.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        },
        Ok(_) => HealthCheck {
            name: "gh".into(),
            ok: false,
            detail: "gh is installed but no account is logged in — run `gh auth login`".into(),
        },
        Err(err) => HealthCheck {
            name: "gh".into(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

fn check_editor(store: &dyn Store) -> HealthCheck {
    match launcher::resolve_editor(store) {
        Ok(editor) => {
            // A configured full path is worth verifying — it is exactly the
            // kind of thing that stops resolving after a settings import from
            // another machine. A bare command name (Rider detection's own
            // `where` lookup, or a name the user expects to be on PATH) has
            // nothing more to check here.
            let looks_like_path = editor.contains('/') || editor.contains('\\');
            let missing = looks_like_path && !std::path::Path::new(&editor).exists();
            HealthCheck {
                name: "editor".into(),
                ok: !missing,
                detail: if missing {
                    format!("configured editor not found on disk: {editor}")
                } else {
                    editor
                },
            }
        }
        Err(err) => HealthCheck {
            name: "editor".into(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

fn check_jira(store: &dyn Store) -> HealthCheck {
    let configured = [
        SETTING_JIRA_BASE_URL,
        SETTING_JIRA_EMAIL,
        SETTING_JIRA_TOKEN,
    ]
    .iter()
    .all(|key| {
        store
            .get_setting(key)
            .ok()
            .flatten()
            .is_some_and(|v| !v.trim().is_empty())
    });
    HealthCheck {
        name: "jira".into(),
        ok: configured,
        detail: if configured {
            "base URL, email, and token are all set".into()
        } else {
            "not configured — optional; only needed for the Jira daemon flow".into()
        },
    }
}

fn check_repo(worktrees: &WorktreeManager) -> HealthCheck {
    match worktrees.repo_info() {
        Ok(Some(info)) => HealthCheck {
            name: "repository".into(),
            ok: true,
            detail: info.path.to_string_lossy().into_owned(),
        },
        Ok(None) => HealthCheck {
            name: "repository".into(),
            ok: false,
            detail: "no repository selected yet".into(),
        },
        Err(err) => HealthCheck {
            name: "repository".into(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::daemon::GhAccount;
    use crate::core::launcher::SETTING_EDITOR_COMMAND;
    use crate::core::store::SqliteStore;
    use crate::core::worktree::GitCli;
    use std::sync::Arc;

    struct MockGh {
        accounts: crate::error::Result<Vec<GhAccount>>,
    }

    impl GhProvider for MockGh {
        fn accounts(&self) -> crate::error::Result<Vec<GhAccount>> {
            match &self.accounts {
                Ok(v) => Ok(v.clone()),
                Err(_) => Err(crate::error::MaestroError::Config {
                    message: "gh not found".into(),
                }),
            }
        }
        fn token(&self, _account: &str) -> crate::error::Result<String> {
            Ok("tok".into())
        }
        fn open_pulls(
            &self,
            _t: &str,
            _s: &str,
        ) -> crate::error::Result<Vec<crate::core::daemon::github::GhPull>> {
            Ok(Vec::new())
        }
        fn pull_comments(
            &self,
            _t: &str,
            _s: &str,
            _n: u64,
        ) -> crate::error::Result<Vec<crate::core::daemon::github::GhComment>> {
            Ok(Vec::new())
        }
    }

    fn worktrees() -> WorktreeManager {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        WorktreeManager::new(Arc::new(GitCli::new()), store, EventBus::new())
    }

    #[test]
    fn gh_check_reports_authenticated_accounts() {
        let gh = MockGh {
            accounts: Ok(vec![GhAccount {
                login: "alice".into(),
                active: true,
            }]),
        };
        let check = check_gh(&gh);
        assert!(check.ok);
        assert!(check.detail.contains("alice"));
    }

    #[test]
    fn gh_check_flags_no_authenticated_account() {
        let gh = MockGh {
            accounts: Ok(vec![]),
        };
        let check = check_gh(&gh);
        assert!(!check.ok);
        assert!(check.detail.contains("gh auth login"));
    }

    #[test]
    fn gh_check_flags_missing_cli() {
        let gh = MockGh {
            accounts: Err(crate::error::MaestroError::Config {
                message: "not found".into(),
            }),
        };
        assert!(!check_gh(&gh).ok);
    }

    #[test]
    fn jira_check_requires_all_three_settings() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!check_jira(&store).ok);

        store
            .set_setting(SETTING_JIRA_BASE_URL, "https://org.atlassian.net")
            .unwrap();
        store.set_setting(SETTING_JIRA_EMAIL, "me@org.com").unwrap();
        assert!(!check_jira(&store).ok, "token still missing");

        store.set_setting(SETTING_JIRA_TOKEN, "tok").unwrap();
        assert!(check_jira(&store).ok);
    }

    #[test]
    fn repo_check_reports_none_selected() {
        let check = check_repo(&worktrees());
        assert!(!check.ok);
        assert!(check.detail.contains("no repository"));
    }

    #[test]
    fn editor_check_flags_a_configured_path_that_does_not_exist() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .set_setting(SETTING_EDITOR_COMMAND, "Z:/nowhere/rider64.exe")
            .unwrap();
        let check = check_editor(&store);
        assert!(!check.ok);
        assert!(check.detail.contains("not found on disk"));
    }

    #[test]
    fn run_produces_one_check_per_area() {
        let store = SqliteStore::open_in_memory().unwrap();
        let gh = MockGh {
            accounts: Ok(vec![]),
        };
        let tmp = tempfile::tempdir().unwrap();
        let report = run(&store, &gh, &worktrees(), &tmp.path().join("config.toml"));
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["git", "gh", "editor", "jira", "repository", "config.toml"]
        );
    }

    #[test]
    fn config_check_passes_when_absent_or_valid_and_fails_on_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let absent = check_config(&path);
        assert!(absent.ok);
        assert!(absent.detail.contains("defaults"));

        std::fs::write(&path, "daemon_poll_minutes = 7\n").unwrap();
        assert!(check_config(&path).ok);

        std::fs::write(&path, "daemon_poll_minutes = [not toml").unwrap();
        let broken = check_config(&path);
        assert!(!broken.ok);
        assert!(broken.detail.contains("ignored"), "{}", broken.detail);
    }
}
