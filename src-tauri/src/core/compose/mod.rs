//! One-shot text generation for the PR workflow: commit messages, PR
//! descriptions, and review-comment replies. Uses the Claude CLI in print mode
//! (`claude -p`) — no session, no transcript, just prompt in → text out. The
//! prompts are the ordinary editable templates in `~/.maestro/prompts/`
//! (`commit-message`, `pr-description`, `pr-reply`).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::core::prompts::PromptManager;
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// Model for one-shot generation (empty = the CLI's default).
pub const SETTING_COMPOSE_MODEL: &str = "compose_model";

/// Diffs are context, not gospel — cap what we feed the model.
const MAX_DIFF_CHARS: usize = 60_000;
/// Print-mode generation deadline.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(120);

/// A generated PR draft: the title line and the markdown body.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PrDraft {
    pub title: String,
    pub body: String,
}

/// The text-generation seam — tests swap in a canned generator.
pub trait TextGen: Send + Sync {
    fn generate(&self, cwd: &Path, prompt: &str, model: Option<&str>) -> Result<String>;
}

/// `claude -p` (print mode). The prompt travels via stdin so a large diff never
/// hits the command-line length limit.
pub struct ClaudeCliGen;

impl TextGen for ClaudeCliGen {
    fn generate(&self, cwd: &Path, prompt: &str, model: Option<&str>) -> Result<String> {
        let mut args: Vec<String> = vec!["-p".into(), "--output-format".into(), "text".into()];
        if let Some(model) = model {
            args.push("--model".into());
            args.push(model.to_string());
        }
        let mut cmd = new_command("claude");
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().or_else(|_| {
            // npm installs expose a .cmd shim rather than an .exe on Windows.
            let mut cmd = new_command("claude.cmd");
            cmd.args(&args)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.spawn()
        }).map_err(|err| MaestroError::Config {
            message: format!("could not launch the Claude CLI (`claude`): {err} — is Claude Code installed and on PATH?"),
        })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(prompt.as_bytes())
                .map_err(|err| MaestroError::Config {
                    message: format!("could not send the prompt to claude: {err}"),
                })?;
        }
        drop(child.stdin.take());

        // Manual deadline: print mode normally answers in seconds, but a hung
        // network must not wedge the IPC thread forever.
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() > GENERATION_TIMEOUT {
                        kill_tree(&mut child);
                        return Err(MaestroError::Config {
                            message: "generation timed out after 120s".into(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(err) => {
                    return Err(MaestroError::Config {
                        message: format!("claude did not finish: {err}"),
                    });
                }
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|err| MaestroError::Config {
                message: format!("claude did not finish: {err}"),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MaestroError::Config {
                message: format!(
                    "claude -p failed: {}",
                    if stderr.trim().is_empty() {
                        stdout.chars().take(300).collect::<String>()
                    } else {
                        stderr.trim().chars().take(300).collect::<String>()
                    }
                ),
            });
        }
        if stdout.is_empty() {
            return Err(MaestroError::Config {
                message: "claude returned an empty answer".into(),
            });
        }
        Ok(stdout)
    }
}

pub struct ComposeManager {
    store: Arc<dyn Store>,
    worktrees: Arc<WorktreeManager>,
    prompts: Arc<PromptManager>,
    text_gen: Arc<dyn TextGen>,
}

impl ComposeManager {
    pub fn new(
        store: Arc<dyn Store>,
        worktrees: Arc<WorktreeManager>,
        prompts: Arc<PromptManager>,
        text_gen: Arc<dyn TextGen>,
    ) -> Self {
        Self {
            store,
            worktrees,
            prompts,
            text_gen,
        }
    }

    /// A commit message for everything uncommitted in `branch`'s worktree.
    pub fn commit_message(&self, branch: &str) -> Result<String> {
        let cwd = self.worktree_path(branch)?;
        let files = git(&cwd, &["status", "--short"])?;
        if files.trim().is_empty() {
            return Err(MaestroError::InvalidData {
                message: "nothing to commit — the worktree is clean".into(),
            });
        }
        // Untracked files never show in `diff HEAD`; add intent-to-add context.
        let diff = truncate(&git(&cwd, &["diff", "HEAD"])?, MAX_DIFF_CHARS);
        let vars = self.common_vars(branch, &files, &diff)?;
        let prompt = self.prompts.render("commit-message", &vars)?;
        let model = self.model();
        self.text_gen.generate(&cwd, &prompt, model.as_deref())
    }

    /// A PR title + body for `branch` against its base.
    pub fn pr_description(&self, branch: &str) -> Result<PrDraft> {
        let cwd = self.worktree_path(branch)?;
        let base = self.base_of(branch)?;
        let commits = git(&cwd, &["log", "--format=%h %s", &format!("{base}..HEAD")])?;
        let files = git(&cwd, &["diff", "--stat", &format!("{base}...HEAD")])?;
        let diff = truncate(
            &git(&cwd, &["diff", &format!("{base}...HEAD")])?,
            MAX_DIFF_CHARS,
        );
        if diff.trim().is_empty() && commits.trim().is_empty() {
            return Err(MaestroError::InvalidData {
                message: format!("branch has no commits over '{base}' — nothing to describe"),
            });
        }
        let mut vars = self.common_vars(branch, &files, &diff)?;
        vars.insert("commits".into(), commits);
        let prompt = self.prompts.render("pr-description", &vars)?;
        let model = self.model();
        let raw = self.text_gen.generate(&cwd, &prompt, model.as_deref())?;
        Ok(parse_pr_draft(&raw))
    }

    /// Draft replies for PR review comments: `comments` come in as
    /// `(comment_id, author, path, body)`, drafts come back keyed by id.
    pub fn reply_drafts(
        &self,
        branch: &str,
        comments: &[(u64, String, String, String)],
    ) -> Result<HashMap<u64, String>> {
        if comments.is_empty() {
            return Ok(HashMap::new());
        }
        let cwd = self.worktree_path(branch)?;
        let base = self.base_of(branch)?;
        let diff = truncate(
            &git(&cwd, &["diff", &format!("{base}...HEAD")])?,
            MAX_DIFF_CHARS,
        );
        let listing = comments
            .iter()
            .map(|(id, author, path, body)| {
                format!("[comment {id}] by {author} on `{path}`:\n{body}\n")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut vars = self.common_vars(branch, "", &diff)?;
        vars.insert("comments".into(), listing);
        let prompt = self.prompts.render("pr-reply", &vars)?;
        let model = self.model();
        let raw = self.text_gen.generate(&cwd, &prompt, model.as_deref())?;
        Ok(parse_reply_drafts(&raw, comments))
    }

    fn common_vars(
        &self,
        branch: &str,
        files: &str,
        diff: &str,
    ) -> Result<HashMap<String, String>> {
        let record = self.store.get_branch(branch)?;
        let mut vars = HashMap::new();
        vars.insert("branch".into(), branch.to_string());
        vars.insert(
            "task_id".into(),
            record
                .as_ref()
                .and_then(|b| b.task_id.clone())
                .unwrap_or_default(),
        );
        vars.insert("base".into(), self.base_of(branch)?);
        vars.insert("files".into(), files.to_string());
        vars.insert("diff".into(), diff.to_string());
        Ok(vars)
    }

    fn base_of(&self, branch: &str) -> Result<String> {
        if let Some(base) = self.store.get_branch(branch)?.and_then(|b| b.base_branch) {
            return Ok(base);
        }
        let repo = self
            .worktrees
            .repo_info()?
            .ok_or_else(|| MaestroError::Config {
                message: "no repository selected".into(),
            })?;
        Ok(repo.default_branch)
    }

    fn worktree_path(&self, branch: &str) -> Result<PathBuf> {
        let info = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        Ok(info.path)
    }

    fn model(&self) -> Option<String> {
        self.store
            .get_setting(SETTING_COMPOSE_MODEL)
            .ok()
            .flatten()
            .filter(|m| !m.trim().is_empty())
    }
}

/// "TITLE: xyz\n\nbody…" → PrDraft. A missing TITLE line falls back to the
/// first non-empty line as title.
fn parse_pr_draft(raw: &str) -> PrDraft {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("TITLE:") {
        let mut lines = rest.splitn(2, '\n');
        let title = lines.next().unwrap_or("").trim().to_string();
        let body = lines.next().unwrap_or("").trim().to_string();
        return PrDraft { title, body };
    }
    let mut lines = trimmed.splitn(2, '\n');
    PrDraft {
        title: lines.next().unwrap_or("").trim().to_string(),
        body: lines.next().unwrap_or("").trim().to_string(),
    }
}

/// Parse "[reply to 123]\ntext…" blocks; anything unmatched keeps its comment
/// without a draft (an empty draft, never a made-up one).
fn parse_reply_drafts(
    raw: &str,
    comments: &[(u64, String, String, String)],
) -> HashMap<u64, String> {
    let mut drafts: HashMap<u64, String> = HashMap::new();
    let mut current: Option<u64> = None;
    let mut buffer = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        let marker = trimmed
            .strip_prefix("[reply to ")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|id| id.trim().parse::<u64>().ok());
        if let Some(id) = marker {
            if let Some(prev) = current.take() {
                drafts.insert(prev, buffer.trim().to_string());
            }
            buffer.clear();
            current = Some(id);
        } else if current.is_some() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }
    if let Some(prev) = current.take() {
        drafts.insert(prev, buffer.trim().to_string());
    }
    // Only ids that actually exist — a hallucinated id must not create a reply.
    let known: std::collections::HashSet<u64> = comments.iter().map(|c| c.0).collect();
    drafts.retain(|id, draft| known.contains(id) && !draft.is_empty());
    drafts
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… (diff truncated)", &text[..cut])
}

/// `git` in `cwd`, stdout as a string. Read-only callers only.
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = new_command("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().map_err(|err| MaestroError::Config {
        message: format!("failed to launch git: {err}"),
    })?;
    if !output.status.success() {
        return Err(MaestroError::Config {
            message: format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Kill a spawned CLI and its whole process tree. `claude` runs behind a
/// `cmd.exe` shim on Windows — killing only the direct child would orphan the
/// actual binary, which then sits on the API connection forever.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let mut cmd = new_command("taskkill");
        cmd.args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.status();
    }
    let _ = child.kill();
}

fn new_command(program: &str) -> Command {
    let cmd = Command::new(program);
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut cmd = cmd;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    };
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_draft_parses_title_line_and_body() {
        let draft = parse_pr_draft("TITLE: T-9: add retry logic\n\n**What changed**\n- stuff");
        assert_eq!(draft.title, "T-9: add retry logic");
        assert!(draft.body.starts_with("**What changed**"));

        let fallback = parse_pr_draft("Just a title\nAnd a body");
        assert_eq!(fallback.title, "Just a title");
        assert_eq!(fallback.body, "And a body");
    }

    #[test]
    fn reply_drafts_parse_by_marker_and_drop_unknown_ids() {
        let comments = vec![
            (
                501,
                "rev".to_string(),
                "a.rs".to_string(),
                "why?".to_string(),
            ),
            (
                502,
                "rev".to_string(),
                "b.rs".to_string(),
                "typo".to_string(),
            ),
        ];
        let raw = "[reply to 501]\nBecause of X.\nSee the test.\n\n[reply to 502]\nFixed.\n\n[reply to 999]\nHallucinated.";
        let drafts = parse_reply_drafts(raw, &comments);
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[&501], "Because of X.\nSee the test.");
        assert_eq!(drafts[&502], "Fixed.");
        assert!(!drafts.contains_key(&999));
    }

    #[test]
    #[ignore = "talks to the real claude CLI; run explicitly"]
    fn live_claude_p_roundtrip() {
        let out = ClaudeCliGen
            .generate(Path::new("."), "Say OK and nothing else.", None)
            .expect("claude -p answers");
        assert!(out.contains("OK"), "unexpected answer: {out}");
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        let text = "аб".repeat(10); // multi-byte
        let out = truncate(&text, 7);
        assert!(out.contains("truncated"));
    }
}
