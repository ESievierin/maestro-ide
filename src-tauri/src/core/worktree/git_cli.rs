//! [`GitProvider`] implementation over the `git` CLI.
//!
//! The CLI (not libgit2) is deliberate: worktree behavior must match what the user
//! sees when they run git themselves.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::core::worktree::provider::{BranchStatus, GitProvider, WorktreeEntry};
use crate::error::{GitErrorKind, MaestroError, Result};

pub struct GitCli;

impl GitCli {
    /// Run git with `args` in `cwd`; return stdout on success, a typed error with
    /// stderr attached on failure.
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let output = self.output(cwd, args)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MaestroError::Git {
                kind: GitErrorKind::CommandFailed,
                message: format!("`git {}` failed: {}", args.join(" "), stderr.trim()),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run git and report only whether it exited successfully.
    fn succeeds(&self, cwd: &Path, args: &[&str]) -> Result<bool> {
        Ok(self.output(cwd, args)?.status.success())
    }

    fn output(&self, cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW: don't flash console windows from the GUI app.
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        cmd.output().map_err(|err| MaestroError::Git {
            kind: GitErrorKind::NotInstalled,
            message: format!("failed to launch git: {err}"),
        })
    }
}

impl GitProvider for GitCli {
    fn is_git_repo(&self, path: &Path) -> Result<bool> {
        if !path.is_dir() {
            return Ok(false);
        }
        self.succeeds(path, &["rev-parse", "--is-inside-work-tree"])
    }

    fn default_branch(&self, repo: &Path) -> Result<String> {
        // Prefer the remote's default branch when a remote exists.
        if let Ok(sym) = self.run(
            repo,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        ) {
            if let Some(name) = sym.trim().strip_prefix("origin/") {
                return Ok(name.to_string());
            }
        }
        // Fall back to whatever the primary worktree has checked out.
        let head = self.run(repo, &["symbolic-ref", "--short", "HEAD"])?;
        Ok(head.trim().to_string())
    }

    fn list_branches(&self, repo: &Path) -> Result<Vec<String>> {
        let out = self.run(
            repo,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )?;
        Ok(out.lines().map(str::to_string).collect())
    }

    fn list_remote_branches(&self, repo: &Path) -> Result<Vec<String>> {
        // %(symref) is non-empty for symbolic refs like origin/HEAD — skip those.
        let out = self.run(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)\t%(symref)",
                "refs/remotes",
            ],
        )?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '\t');
                let name = parts.next()?.trim();
                let symref = parts.next().unwrap_or("").trim();
                (!name.is_empty() && symref.is_empty()).then(|| name.to_string())
            })
            .collect())
    }

    fn branch_exists(&self, repo: &Path, branch: &str) -> Result<bool> {
        let refname = format!("refs/heads/{branch}");
        self.succeeds(repo, &["show-ref", "--verify", "--quiet", &refname])
    }

    fn list_worktrees(&self, repo: &Path) -> Result<Vec<WorktreeEntry>> {
        let out = self.run(repo, &["worktree", "list", "--porcelain"])?;
        Ok(parse_worktree_list(&out))
    }

    fn create_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base: Option<&str>,
    ) -> Result<()> {
        let path_str = path.to_string_lossy();
        match base {
            Some(base) => {
                self.run(repo, &["worktree", "add", "-b", branch, &path_str, base])?;
            }
            None => {
                self.run(repo, &["worktree", "add", &path_str, branch])?;
            }
        }
        Ok(())
    }

    fn remove_worktree(&self, repo: &Path, path: &Path, force: bool) -> Result<()> {
        let path_str = path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path_str);
        self.run(repo, &args)?;
        Ok(())
    }

    fn branch_status(&self, worktree: &Path) -> Result<BranchStatus> {
        let out = self.run(worktree, &["status", "--porcelain=v2", "--branch"])?;
        Ok(parse_branch_status(&out))
    }

    fn merge_base_diff(&self, repo: &Path, branch: &str, base: &str) -> Result<String> {
        // `base...branch` diffs from the merge-base, exactly what the diff viewer needs.
        let range = format!("{base}...{branch}");
        self.run(repo, &["diff", &range])
    }
}

/// Parse `git worktree list --porcelain` output. The first entry is the primary
/// working tree.
fn parse_worktree_list(out: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;

    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                is_primary: entries.is_empty(),
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(entry) = current.as_mut() {
                entry.head = head.to_string();
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(entry) = current.as_mut() {
                entry.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            }
        }
        // "detached", "bare", "locked", blank lines: nothing to record.
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    entries
}

/// Parse `git status --porcelain=v2 --branch` output.
fn parse_branch_status(out: &str) -> BranchStatus {
    let mut status = BranchStatus::default();
    for line in out.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    status.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            status.dirty = true;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list() {
        let out = "worktree C:/work/repo\n\
                   HEAD 1111111111111111111111111111111111111111\n\
                   branch refs/heads/main\n\
                   \n\
                   worktree C:/work/repo.worktrees/impl-T-1-x\n\
                   HEAD 2222222222222222222222222222222222222222\n\
                   branch refs/heads/impl/T-1-x\n\
                   \n\
                   worktree C:/work/repo.worktrees/detached\n\
                   HEAD 3333333333333333333333333333333333333333\n\
                   detached\n";
        let entries = parse_worktree_list(out);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_primary);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert!(!entries[1].is_primary);
        assert_eq!(entries[1].branch.as_deref(), Some("impl/T-1-x"));
        assert_eq!(entries[2].branch, None);
    }

    #[test]
    fn parses_clean_status_with_ahead_behind() {
        let out = "# branch.oid 1234\n# branch.head impl/T-1-x\n\
                   # branch.upstream origin/impl/T-1-x\n# branch.ab +3 -1\n";
        let status = parse_branch_status(out);
        assert!(!status.dirty);
        assert_eq!(status.ahead, 3);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn parses_dirty_status() {
        let out = "# branch.oid 1234\n# branch.head main\n\
                   1 .M N... 100644 100644 100644 abc def src/lib.rs\n\
                   ? new-file.txt\n";
        let status = parse_branch_status(out);
        assert!(status.dirty);
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
    }

    /// End-to-end against the real git CLI in a temp directory.
    mod integration {
        use super::super::*;
        use std::fs;

        fn git(cwd: &Path, args: &[&str]) {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("launch git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn init_repo(dir: &Path) {
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "test@maestro.local"]);
            git(dir, &["config", "user.name", "Maestro Test"]);
            fs::write(dir.join("README.md"), "hello\n").expect("write file");
            git(dir, &["add", "."]);
            git(dir, &["commit", "-m", "init"]);
        }

        #[test]
        fn full_worktree_lifecycle() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli;
            assert!(cli.is_git_repo(&repo).expect("is_git_repo"));
            assert!(!cli.is_git_repo(tmp.path()).expect("non-repo"));
            assert_eq!(cli.default_branch(&repo).expect("default"), "main");
            assert!(
                cli.list_remote_branches(&repo).expect("remotes").is_empty(),
                "no remotes in a local test repo"
            );

            // Create a worktree on a new branch.
            let wt = tmp.path().join("wt-impl-t1");
            cli.create_worktree(&repo, &wt, "impl/T-1-x", Some("main"))
                .expect("create worktree");
            assert!(cli.branch_exists(&repo, "impl/T-1-x").expect("exists"));

            let entries = cli.list_worktrees(&repo).expect("list");
            assert_eq!(entries.len(), 2);
            assert!(entries[0].is_primary);
            assert_eq!(entries[1].branch.as_deref(), Some("impl/T-1-x"));

            // Clean → dirty transition.
            assert!(!cli.branch_status(&wt).expect("status").dirty);
            fs::write(wt.join("new.txt"), "x\n").expect("write");
            assert!(cli.branch_status(&wt).expect("status").dirty);

            // Commit in the worktree; diff vs merge-base with main shows it.
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-m", "change"]);
            let diff = cli
                .merge_base_diff(&repo, "impl/T-1-x", "main")
                .expect("diff");
            assert!(diff.contains("new.txt"), "diff should mention new.txt");

            // Removal (clean tree, no force needed).
            cli.remove_worktree(&repo, &wt, false).expect("remove");
            assert_eq!(cli.list_worktrees(&repo).expect("list").len(), 1);
            // The branch survives worktree removal.
            assert!(cli.branch_exists(&repo, "impl/T-1-x").expect("exists"));
        }
    }
}
