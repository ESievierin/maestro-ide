//! Prompt rendering for the PR workflow: commit messages and PR descriptions.
//!
//! This module only gathers git context (status/diff/commits) and renders
//! the editable templates in `~/.maestro/prompts/` — it never calls an LLM
//! itself. The *asking* happens through a real session on the frontend
//! (`src/utils/agentAsk.ts`), spawned with `resume_from` the branch's own
//! implementation session when one exists, so generation sees the same
//! context an agent working on the branch would — not just a diff.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::core::prompts::PromptManager;
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// Diffs are context, not gospel — cap what we feed the model.
const MAX_DIFF_CHARS: usize = 60_000;

pub struct ComposeManager {
    store: Arc<dyn Store>,
    worktrees: Arc<WorktreeManager>,
    prompts: Arc<PromptManager>,
}

impl ComposeManager {
    pub fn new(
        store: Arc<dyn Store>,
        worktrees: Arc<WorktreeManager>,
        prompts: Arc<PromptManager>,
    ) -> Self {
        Self {
            store,
            worktrees,
            prompts,
        }
    }

    /// Render the "commit-message" prompt for everything uncommitted in
    /// `branch`'s worktree. `base` overrides the stored/default base when
    /// given (kept in sync with whatever the Create PR dialog has selected).
    pub fn commit_prompt(&self, branch: &str, base: Option<&str>) -> Result<String> {
        let cwd = self.worktree_path(branch)?;
        let files = git(&cwd, &["status", "--short"])?;
        if files.trim().is_empty() {
            return Err(MaestroError::InvalidData {
                message: "nothing to commit — the worktree is clean".into(),
            });
        }
        let diff = truncate(&git(&cwd, &["diff", "HEAD"])?, MAX_DIFF_CHARS);
        let resolved_base = self.resolve_base(branch, base)?;
        let vars = self.common_vars(branch, &resolved_base, &files, &diff)?;
        self.prompts.render("commit-message", &vars)
    }

    /// Render the "pr-description" prompt for `branch` against `base` (falls
    /// back to the stored/default base when absent). The diff is the working
    /// tree against the merge-base — committed **and** uncommitted changes —
    /// since a PR about to be opened is described by what it will contain,
    /// not just whatever happens to be committed yet. Returns the base that
    /// was actually used, so the caller can reflect an auto-detected one.
    pub fn pr_prompt(&self, branch: &str, base: Option<&str>) -> Result<(String, String)> {
        let (resolved_base, vars) = self.branch_context(branch, base)?;
        let prompt = self.prompts.render("pr-description", &vars)?;
        Ok((resolved_base, prompt))
    }

    /// Render the "review-guide" prompt: same branch context as a PR
    /// description, but asking for a machine-readable review roadmap instead
    /// of prose (the frontend parses the reply as JSON).
    pub fn guide_prompt(&self, branch: &str) -> Result<String> {
        let (_, vars) = self.branch_context(branch, None)?;
        self.prompts.render("review-guide", &vars)
    }

    /// Everything a whole-branch prompt needs: commits, file list (tracked
    /// stat + untracked names), truncated working-tree diff vs the merge-base.
    fn branch_context(
        &self,
        branch: &str,
        base: Option<&str>,
    ) -> Result<(String, HashMap<String, String>)> {
        let cwd = self.worktree_path(branch)?;
        let resolved_base = self.resolve_base(branch, base)?;
        let merge_base = git(&cwd, &["merge-base", &resolved_base, "HEAD"])?
            .trim()
            .to_string();
        let commits = git(
            &cwd,
            &["log", "--format=%h %s", &format!("{resolved_base}..HEAD")],
        )
        .unwrap_or_default();
        // `git diff` never mentions untracked files, no matter what it's compared
        // against — list them by name alongside the tracked-file stat, so a brand
        // new file at least gets acknowledged even though its content doesn't.
        let stat = git(&cwd, &["diff", "--stat", &merge_base])?;
        let untracked: String = git(&cwd, &["status", "--short"])?
            .lines()
            .filter(|line| line.starts_with("??"))
            .collect::<Vec<_>>()
            .join("\n");
        let files = if untracked.is_empty() {
            stat
        } else {
            format!("{stat}\n{untracked}")
        };
        let diff = truncate(&git(&cwd, &["diff", &merge_base])?, MAX_DIFF_CHARS);
        if diff.trim().is_empty() {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "no changes on this branch relative to '{resolved_base}' (including \
                     uncommitted ones) — nothing to describe"
                ),
            });
        }
        let mut vars = self.common_vars(branch, &resolved_base, &files, &diff)?;
        vars.insert("commits".into(), commits);
        Ok((resolved_base, vars))
    }

    fn common_vars(
        &self,
        branch: &str,
        base: &str,
        files: &str,
        diff: &str,
    ) -> Result<HashMap<String, String>> {
        let record = self.store.get_branch(branch)?;
        let mut vars = HashMap::new();
        vars.insert("branch".into(), branch.to_string());
        vars.insert(
            "task_id".into(),
            record.and_then(|b| b.task_id).unwrap_or_default(),
        );
        vars.insert("base".into(), base.to_string());
        vars.insert("files".into(), files.to_string());
        vars.insert("diff".into(), diff.to_string());
        Ok(vars)
    }

    /// The base to diff/describe against: an explicit override, else the
    /// branch's stored base, else the repository's default branch.
    fn resolve_base(&self, branch: &str, base: Option<&str>) -> Result<String> {
        if let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) {
            return Ok(base.to_string());
        }
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
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_char_boundary_safe() {
        let text = "аб".repeat(10); // multi-byte
        let out = truncate(&text, 7);
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncation_is_a_no_op_under_the_limit() {
        assert_eq!(truncate("short", 100), "short");
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::store::SqliteStore;
    use crate::core::worktree::{CreateWorktreeRequest, GitCli, WorktreeManager};
    use std::fs;

    fn git_cmd(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git_cmd(dir, &["init", "-b", "main"]);
        git_cmd(dir, &["config", "user.email", "t@t.t"]);
        git_cmd(dir, &["config", "user.name", "t"]);
        fs::write(dir.join("a.txt"), "base\n").unwrap();
        git_cmd(dir, &["add", "-A"]);
        git_cmd(dir, &["commit", "-m", "init"]);
    }

    /// A real repo with one feature-branch worktree, and a `ComposeManager`
    /// wired to it — real git, not a mock, since this module's whole job is
    /// shelling out to git correctly.
    fn setup() -> (ComposeManager, PathBuf, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let worktrees = Arc::new(WorktreeManager::new(
            Arc::new(GitCli::new()),
            store.clone(),
            EventBus::new(),
        ));
        worktrees.set_repo(&repo).unwrap();
        let info = worktrees
            .create(CreateWorktreeRequest {
                existing_branch: None,
                kind: Some("impl".into()),
                task_id: Some("T-1".into()),
                slug: Some("compose test".into()),
                base: Some("main".into()),
            })
            .unwrap();
        let branch = info.branch.unwrap();

        let prompts = Arc::new(PromptManager::new(tmp.path().join("prompts")).unwrap());
        let compose = ComposeManager::new(store, worktrees, prompts);
        (compose, info.path, branch, tmp)
    }

    #[test]
    fn commit_prompt_rejects_a_clean_worktree() {
        let (compose, _wt, branch, _tmp) = setup();
        let err = compose.commit_prompt(&branch, None).unwrap_err();
        assert!(err.to_string().contains("clean"));
    }

    #[test]
    fn commit_prompt_renders_status_task_id_base_and_diff() {
        let (compose, wt, branch, _tmp) = setup();
        fs::write(wt.join("a.txt"), "changed\n").unwrap();
        let prompt = compose.commit_prompt(&branch, None).unwrap();
        assert!(prompt.contains("a.txt"));
        assert!(prompt.contains("-base"));
        assert!(prompt.contains("+changed"));
        assert!(prompt.contains("main"), "the base shows up in the prompt");
        assert!(prompt.contains("T-1"), "the stored task id shows up too");
    }

    #[test]
    fn pr_prompt_includes_uncommitted_changes() {
        let (compose, wt, branch, _tmp) = setup();
        // Uncommitted edit to a *tracked* file — `git diff` never shows a
        // brand-new untracked file's content, committed or not.
        fs::write(wt.join("a.txt"), "uncommitted work\n").unwrap();
        let (base, prompt) = compose.pr_prompt(&branch, None).unwrap();
        assert_eq!(base, "main");
        assert!(prompt.contains("uncommitted work"));
    }

    #[test]
    fn pr_prompt_lists_new_untracked_files_by_name() {
        let (compose, wt, branch, _tmp) = setup();
        fs::write(wt.join("b.txt"), "brand new\n").unwrap();
        // The file itself is untracked, but the branch still needs a change to
        // describe (an untracked file alone diffs to nothing).
        fs::write(wt.join("a.txt"), "changed too\n").unwrap();
        let (_base, prompt) = compose.pr_prompt(&branch, None).unwrap();
        assert!(prompt.contains("b.txt"), "named even without its content");
    }

    #[test]
    fn pr_prompt_honors_an_explicit_base_override() {
        let (compose, wt, branch, tmp) = setup();
        // A `develop` branch exists too; the caller explicitly asks to
        // compare against it instead of whatever was stored at creation.
        git_cmd(&tmp.path().join("repo"), &["branch", "develop"]);
        fs::write(wt.join("a.txt"), "on the branch\n").unwrap();
        let (base, prompt) = compose.pr_prompt(&branch, Some("develop")).unwrap();
        assert_eq!(base, "develop");
        assert!(prompt.contains("on the branch"));
    }

    #[test]
    fn pr_prompt_refuses_when_there_is_nothing_to_describe() {
        let (compose, _wt, branch, _tmp) = setup();
        let err = compose.pr_prompt(&branch, None).unwrap_err();
        assert!(err.to_string().contains("nothing to describe"));
    }
}
