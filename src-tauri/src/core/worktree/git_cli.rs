//! [`GitProvider`] implementation over the `git` CLI.
//!
//! The CLI (not libgit2) is deliberate: worktree behavior must match what the user
//! sees when they run git themselves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::worktree::provider::{
    BlameLine, BranchStatus, ChangedFile, GitProvider, LogEntry, MergeOutcome, Snapshot,
    WorktreeEntry,
};
use crate::error::{GitErrorKind, MaestroError, Result};

/// How long a fetched base ref counts as fresh. Diff refreshes happen far more
/// often than a base branch moves; a network round-trip per refresh would make
/// the diff viewer feel sluggish for no informational gain.
const FETCH_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct GitCli {
    /// Last successful-ish fetch per (repo, refspec) — see [`FETCH_TTL`].
    fetched: Mutex<HashMap<(PathBuf, String), Instant>>,
    /// Serializes every git subprocess this runs, across every worktree.
    /// git commands are not safe to run concurrently against the same
    /// repository — even a nominally read-only `status` can rewrite
    /// `.git/index` as a stat-cache refresh, so two commands landing at the
    /// same instant (a user's merge/sync click racing the sidebar's
    /// background status poll, say) can fail with "could not write index"
    /// even though neither one is doing anything wrong on its own. A single
    /// global lock is simpler and safer than reasoning about which git
    /// subcommands are "read-only enough" to skip it, and every git call
    /// this app makes is small and local — full serialization costs
    /// nothing a human notices.
    exec_lock: Mutex<()>,
}

impl GitCli {
    pub fn new() -> Self {
        Self::default()
    }
}

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

    /// Configured remote names (`origin`, etc.), empty for a purely local repo.
    fn remotes(&self, cwd: &Path) -> Vec<String> {
        self.run(cwd, &["remote"])
            .map(|out| out.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn output(&self, cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
        // Held across the whole spawn-to-exit lifecycle, on the same
        // blocking-threadpool thread every Tauri command already runs on —
        // blocking here is exactly what that pool is for. See `exec_lock`'s
        // own doc comment for why this exists at all.
        let _exec_guard = self
            .exec_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

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

    fn last_commit_subject(&self, worktree: &Path) -> Result<Option<String>> {
        let out = self.run(worktree, &["log", "-1", "--format=%s"])?;
        let subject = out.trim();
        Ok((!subject.is_empty()).then(|| subject.to_string()))
    }

    fn merge_base_diff(&self, repo: &Path, branch: &str, base: &str) -> Result<String> {
        // `base...branch` diffs from the merge-base, exactly what the diff viewer needs.
        // `--ignore-cr-at-eol`: a repo without consistent .gitattributes can have the
        // same line committed with and without a trailing \r on either side of the
        // range — without this flag that alone makes git diff every line in the file.
        let range = format!("{base}...{branch}");
        self.run(repo, &["diff", "--ignore-cr-at-eol", &range])
    }

    fn merge_base(&self, repo: &Path, base: &str, branch: &str) -> Result<String> {
        Ok(self
            .run(repo, &["merge-base", base, branch])?
            .trim()
            .to_string())
    }

    fn fresh_base_ref(&self, repo: &Path, base: &str) -> String {
        let remotes = self.remotes(repo);
        if remotes.is_empty() {
            return base.to_string();
        }
        // `base` may already be `<remote>/<rest>` (picked from the remote-branch
        // list at worktree-creation time) — recognize that against the repo's
        // actual remotes rather than guessing from the first '/', since a plain
        // local branch name can itself contain slashes (`feature/x`).
        let (remote, short) = remotes
            .iter()
            .find_map(|r| {
                base.strip_prefix(&format!("{r}/"))
                    .map(|rest| (r.clone(), rest.to_string()))
            })
            .unwrap_or_else(|| {
                let remote = if remotes.iter().any(|r| r == "origin") {
                    "origin".to_string()
                } else {
                    remotes[0].clone()
                };
                (remote, base.to_string())
            });
        // Best-effort: an offline machine or a branch with no upstream must never
        // break the diff — just fall through to whatever is already on disk.
        // Throttled: at most one network round-trip per FETCH_TTL per ref, so
        // rapid diff refreshes stay local-fast.
        let key = (repo.to_path_buf(), format!("{remote}/{short}"));
        let stale = self
            .fetched
            .lock()
            .map(|m| {
                m.get(&key)
                    .map(|at| at.elapsed() > FETCH_TTL)
                    .unwrap_or(true)
            })
            .unwrap_or(true);
        if stale {
            let _ = self.run(repo, &["fetch", "--quiet", &remote, &short]);
            if let Ok(mut m) = self.fetched.lock() {
                m.insert(key, Instant::now());
            }
        }
        let candidate = format!("{remote}/{short}");
        let exists = self
            .succeeds(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{candidate}^{{commit}}"),
                ],
            )
            .unwrap_or(false);
        if exists {
            candidate
        } else {
            base.to_string()
        }
    }

    fn changed_files(&self, repo: &Path, branch: &str, base: &str) -> Result<Vec<ChangedFile>> {
        let range = format!("{base}...{branch}");
        let out = self.run(
            repo,
            &["diff", "--ignore-cr-at-eol", "--name-status", "-M", &range],
        )?;
        Ok(parse_name_status(&out))
    }

    fn show_file(&self, repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        if !self.succeeds(repo, &["cat-file", "-e", &spec])? {
            return Ok(None);
        }
        Ok(Some(self.run(repo, &["show", &spec])?))
    }

    fn list_files(&self, worktree: &Path) -> Result<Vec<String>> {
        let out = self.run(
            worktree,
            &["ls-files", "--cached", "--others", "--exclude-standard"],
        )?;
        Ok(out.lines().map(str::to_string).collect())
    }

    fn blame_range(
        &self,
        worktree: &Path,
        path: &str,
        start: u32,
        end: u32,
    ) -> Result<Vec<BlameLine>> {
        let range = format!("{start},{end}");
        let out = self.run(
            worktree,
            &["blame", "--line-porcelain", "-L", &range, "--", path],
        )?;
        Ok(parse_blame(&out))
    }

    fn worktree_diff(&self, worktree: &Path, merge_base: &str) -> Result<String> {
        self.run(worktree, &["diff", "--ignore-cr-at-eol", merge_base])
    }

    fn worktree_changed_files(
        &self,
        worktree: &Path,
        merge_base: &str,
    ) -> Result<Vec<ChangedFile>> {
        let out = self.run(
            worktree,
            &[
                "diff",
                "--ignore-cr-at-eol",
                "--name-status",
                "-M",
                merge_base,
            ],
        )?;
        let mut files = parse_name_status(&out);

        // Untracked files don't appear in `git diff` — pull them from status.
        let status = self.run(worktree, &["status", "--porcelain=v2"])?;
        for line in status.lines() {
            if let Some(path) = line.strip_prefix("? ") {
                files.push(ChangedFile {
                    path: path.to_string(),
                    status: "A".into(),
                    old_path: None,
                });
            }
        }
        Ok(files)
    }

    fn merge_branch(&self, target_worktree: &Path, source_branch: &str) -> Result<MergeOutcome> {
        let output = self.output(
            target_worktree,
            &["merge", "--no-ff", "--no-edit", source_branch],
        )?;
        if output.status.success() {
            return Ok(MergeOutcome {
                merged: true,
                conflicts: Vec::new(),
                message: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            });
        }
        // Conflicts leave `U` (unmerged) entries in `git diff --diff-filter=U`;
        // any other failure (e.g. a merge already in progress) leaves this empty,
        // and `message` carries git's own explanation instead.
        let conflicts = self
            .run(target_worktree, &["diff", "--name-only", "--diff-filter=U"])
            .map(|out| out.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Ok(MergeOutcome {
            merged: false,
            conflicts,
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }

    fn switch_branch(&self, worktree: &Path, branch: &str) -> Result<()> {
        self.run(worktree, &["switch", branch])?;
        Ok(())
    }

    fn discard_changes(&self, worktree: &Path) -> Result<()> {
        self.run(worktree, &["reset", "--hard"])?;
        self.run(worktree, &["clean", "-fd"])?;
        Ok(())
    }

    fn fetch_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        let refspec = format!("{branch}:{branch}");
        match self.run(repo, &["fetch", "origin", &refspec]) {
            Ok(_) => Ok(()),
            // Git refuses to fetch into a currently checked-out branch; the
            // checkout means the branch exists locally, so a plain fetch is all
            // the freshness we can (and need to) get here.
            Err(_) => self.run(repo, &["fetch", "origin", branch]).map(|_| ()),
        }
    }

    fn commit_all(&self, worktree: &Path, message: &str) -> Result<String> {
        self.run(worktree, &["add", "-A"])?;
        self.run(worktree, &["commit", "-m", message])?;
        Ok(self
            .run(worktree, &["log", "-1", "--format=%h %s"])?
            .trim()
            .to_string())
    }

    fn snapshot_push(&self, worktree: &Path, label: &str) -> Result<()> {
        let message = format!("{SNAPSHOT_PREFIX}{label}");
        // `stash push` records the state but also reverts the working tree;
        // an immediate `apply` puts it back, leaving the snapshot behind. This
        // is the standard checkpoint idiom — `stash create` would avoid the
        // round-trip but cannot capture untracked files.
        let out = self.run(
            worktree,
            &["stash", "push", "--include-untracked", "-m", &message],
        )?;
        if out.contains("No local changes") {
            return Err(MaestroError::Git {
                kind: GitErrorKind::InvalidInput,
                message: "nothing to snapshot — the worktree has no uncommitted changes".into(),
            });
        }
        self.run(worktree, &["stash", "apply", "--index", "stash@{0}"])
            .or_else(|_| {
                // --index can fail when untracked files are involved; a plain
                // apply restores contents without the staged/unstaged split.
                self.run(worktree, &["stash", "apply", "stash@{0}"])
            })?;
        Ok(())
    }

    fn snapshot_list(&self, worktree: &Path) -> Result<Vec<Snapshot>> {
        let out = self.run(worktree, &["stash", "list", "--format=%gd\x1f%ci\x1f%gs"])?;
        Ok(parse_snapshot_list(&out))
    }

    fn snapshot_restore(&self, worktree: &Path, id: &str) -> Result<()> {
        // Rollback semantics: the current uncommitted state is replaced by the
        // snapshot, not merged with it. The caller has already confirmed the
        // discard; the snapshot itself is kept so it can be restored again.
        self.run(worktree, &["reset", "--hard"])?;
        self.run(worktree, &["clean", "-fd"])?;
        self.run(worktree, &["stash", "apply", id])?;
        Ok(())
    }

    fn snapshot_drop(&self, worktree: &Path, id: &str) -> Result<()> {
        self.run(worktree, &["stash", "drop", id])?;
        Ok(())
    }

    fn push_branch(&self, worktree: &Path, branch: &str) -> Result<String> {
        let remotes = self.remotes(worktree);
        let remote = if remotes.iter().any(|r| r == "origin") {
            "origin".to_string()
        } else {
            remotes.first().cloned().ok_or_else(|| MaestroError::Git {
                kind: GitErrorKind::InvalidInput,
                message: "no remote configured — nothing to push to".into(),
            })?
        };
        // git push reports progress on *stderr* even on success; collect both.
        let output = self.output(worktree, &["push", "-u", &remote, branch])?;
        let mut report = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !report.is_empty() {
                report.push('\n');
            }
            report.push_str(stderr.trim());
        }
        if !output.status.success() {
            return Err(MaestroError::Git {
                kind: GitErrorKind::CommandFailed,
                message: format!("`git push` failed: {report}"),
            });
        }
        Ok(report)
    }

    fn branch_log(
        &self,
        repo: &Path,
        branch: &str,
        base: &str,
        limit: usize,
    ) -> Result<Vec<LogEntry>> {
        let range = format!("{base}..{branch}");
        let count = format!("-{limit}");
        let out = self.run(
            repo,
            &[
                "log",
                &count,
                "--format=%h\x1f%s\x1f%an\x1f%ad",
                "--date=short",
                &range,
            ],
        )?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\x1f');
                Some(LogEntry {
                    sha: parts.next()?.trim().to_string(),
                    subject: parts.next()?.trim().to_string(),
                    author: parts.next()?.trim().to_string(),
                    date: parts.next()?.trim().to_string(),
                })
            })
            .collect())
    }
}

/// Marker distinguishing Maestro snapshots from the user's own stashes.
const SNAPSHOT_PREFIX: &str = "maestro-snapshot: ";

/// Parse `git stash list --format=%gd<US>%ci<US>%gs`, keeping only Maestro
/// snapshots. `%gs` looks like "On <branch>: maestro-snapshot: <label>".
fn parse_snapshot_list(out: &str) -> Vec<Snapshot> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            let id = parts.next()?.trim();
            let created_at = parts.next()?.trim();
            let subject = parts.next()?.trim();
            let label_start = subject.find(SNAPSHOT_PREFIX)?;
            let label = subject[label_start + SNAPSHOT_PREFIX.len()..].to_string();
            Some(Snapshot {
                id: id.to_string(),
                label,
                created_at: created_at.to_string(),
            })
        })
        .collect()
}

/// Parse `git diff --name-status -M` output: `M\tpath` or `R100\told\tnew`.
fn parse_name_status(out: &str) -> Vec<ChangedFile> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status_raw = parts.next()?.trim();
            if status_raw.is_empty() {
                return None;
            }
            let status: String = status_raw.chars().take(1).collect();
            let first = parts.next()?.to_string();
            match parts.next() {
                // Rename/copy: "R100\told\tnew" — the file now lives at `new`.
                Some(new_path) => Some(ChangedFile {
                    path: new_path.to_string(),
                    status,
                    old_path: Some(first),
                }),
                None => Some(ChangedFile {
                    path: first,
                    status,
                    old_path: None,
                }),
            }
        })
        .collect()
}

/// Parse `git blame --line-porcelain` output.
fn parse_blame(out: &str) -> Vec<BlameLine> {
    let mut lines = Vec::new();
    let mut sha = String::new();
    let mut line_no = 0u32;
    let mut author = String::new();
    let mut summary = String::new();

    for raw in out.lines() {
        if let Some(content) = raw.strip_prefix('\t') {
            lines.push(BlameLine {
                sha: sha.chars().take(8).collect(),
                author: author.clone(),
                summary: summary.clone(),
                line: line_no,
                content: content.to_string(),
            });
        } else if let Some(rest) = raw.strip_prefix("author ") {
            author = rest.to_string();
        } else if let Some(rest) = raw.strip_prefix("summary ") {
            summary = rest.to_string();
        } else if !raw.starts_with(char::is_whitespace) {
            // Header line: "<sha> <orig-line> <final-line> [<group-size>]".
            let mut parts = raw.split_whitespace();
            if let (Some(first), Some(_), Some(final_line)) =
                (parts.next(), parts.next(), parts.next())
            {
                if first.len() >= 8 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                    sha = first.to_string();
                    line_no = final_line.parse().unwrap_or(0);
                }
            }
        }
    }
    lines
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
    fn parses_name_status_with_renames() {
        let out = "M\tsrc/lib.rs\nA\tsrc/new.rs\nD\told.txt\nR087\tsrc/a.rs\tsrc/b.rs\n";
        let files = parse_name_status(out);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].status, "M");
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[3].status, "R");
        assert_eq!(files[3].path, "src/b.rs");
        assert_eq!(files[3].old_path.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn parses_line_porcelain_blame() {
        let out = "abcdef1234567890abcdef1234567890abcdef12 3 5 2\n\
                   author Alice\n\
                   author-mail <a@x>\n\
                   summary add feature\n\
                   filename src/lib.rs\n\
                   \tlet x = 1;\n\
                   abcdef1234567890abcdef1234567890abcdef12 4 6\n\
                   author Alice\n\
                   summary add feature\n\
                   filename src/lib.rs\n\
                   \tlet y = 2;\n";
        let lines = parse_blame(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].sha, "abcdef12");
        assert_eq!(lines[0].author, "Alice");
        assert_eq!(lines[0].summary, "add feature");
        assert_eq!(lines[0].line, 5);
        assert_eq!(lines[0].content, "let x = 1;");
        assert_eq!(lines[1].line, 6);
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

            let cli = GitCli::new();
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

            // T5 surface: merge-base, changed files, file contents at revs, blame.
            let mb = cli.merge_base(&repo, "main", "impl/T-1-x").expect("mb");
            assert_eq!(mb.len(), 40, "full commit oid");
            let files = cli
                .changed_files(&repo, "impl/T-1-x", "main")
                .expect("changed files");
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].status, "A");
            assert_eq!(files[0].path, "new.txt");
            assert_eq!(
                cli.show_file(&repo, &mb, "new.txt").expect("show old"),
                None,
                "file did not exist at merge-base"
            );
            assert_eq!(
                cli.show_file(&repo, "impl/T-1-x", "new.txt")
                    .expect("show new")
                    .as_deref(),
                Some("x\n")
            );
            let blame = cli.blame_range(&wt, "new.txt", 1, 1).expect("blame");
            assert_eq!(blame.len(), 1);
            assert_eq!(blame[0].author, "Maestro Test");
            assert_eq!(blame[0].line, 1);

            // Working-tree scope: uncommitted edit + untracked file both visible.
            fs::write(wt.join("new.txt"), "x\nedited\n").expect("edit");
            fs::write(wt.join("untracked.txt"), "u\n").expect("untracked");
            let mb = cli.merge_base(&repo, "main", "impl/T-1-x").expect("mb");
            let wt_files = cli
                .worktree_changed_files(&wt, &mb)
                .expect("worktree files");
            assert!(
                wt_files
                    .iter()
                    .any(|f| f.path == "new.txt" && f.status == "A"),
                "committed+edited file present: {wt_files:?}"
            );
            assert!(
                wt_files
                    .iter()
                    .any(|f| f.path == "untracked.txt" && f.status == "A"),
                "untracked file reported as added: {wt_files:?}"
            );
            let wt_diff = cli.worktree_diff(&wt, &mb).expect("worktree diff");
            assert!(wt_diff.contains("edited"), "uncommitted change in diff");
            fs::remove_file(wt.join("untracked.txt")).expect("cleanup");
            fs::write(wt.join("new.txt"), "x\n").expect("restore");

            // Removal (clean tree, no force needed).
            cli.remove_worktree(&repo, &wt, false).expect("remove");
            assert_eq!(cli.list_worktrees(&repo).expect("list").len(), 1);
            // The branch survives worktree removal.
            assert!(cli.branch_exists(&repo, "impl/T-1-x").expect("exists"));
        }

        #[test]
        fn merge_branch_fast_forwards_cleanly() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            let wt = tmp.path().join("wt");
            cli.create_worktree(&repo, &wt, "impl/T-1-x", Some("main"))
                .expect("create worktree");
            fs::write(wt.join("new.txt"), "x\n").expect("write");
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-m", "add new.txt"]);

            // `repo` is the primary worktree — "main" is checked out there.
            let outcome = cli.merge_branch(&repo, "impl/T-1-x").expect("merge");
            assert!(outcome.merged, "{outcome:?}");
            assert!(outcome.conflicts.is_empty());
            assert!(repo.join("new.txt").exists(), "merge landed the new file");
        }

        #[test]
        fn merge_branch_reports_conflicting_files_and_leaves_markers() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            let wt = tmp.path().join("wt");
            cli.create_worktree(&repo, &wt, "impl/T-1-x", Some("main"))
                .expect("create worktree");

            // Two branches edit the same line differently: main gets one edit,
            // the feature branch (already forked before that) gets another.
            fs::write(wt.join("README.md"), "feature change\n").expect("write");
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-m", "feature edits README"]);

            fs::write(repo.join("README.md"), "main change\n").expect("write");
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-m", "main edits README"]);

            let outcome = cli.merge_branch(&repo, "impl/T-1-x").expect("merge");
            assert!(!outcome.merged, "{outcome:?}");
            assert_eq!(outcome.conflicts, vec!["README.md".to_string()]);

            // git's own working tree really is mid-conflict, not a fabricated result.
            let contents = fs::read_to_string(repo.join("README.md")).expect("read");
            assert!(contents.contains("<<<<<<<"), "{contents}");
            git(&repo, &["merge", "--abort"]);
        }

        /// Snapshot → wreck the worktree → restore: the pre-wreck state is back,
        /// untracked files included, and the snapshot survives for another round.
        #[test]
        fn snapshot_round_trip_restores_tracked_and_untracked_state() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            // Nothing to snapshot on a clean tree — refused, not a phantom entry.
            assert!(cli.snapshot_push(&repo, "empty").is_err());

            fs::write(repo.join("README.md"), "good agent work\n").expect("write");
            fs::write(repo.join("notes.txt"), "untracked but wanted\n").expect("write");
            cli.snapshot_push(&repo, "before risky attempt")
                .expect("snapshot");

            // The working tree's *content* is untouched by taking a snapshot.
            // (Byte-level, autocrlf may re-materialize tracked files with CRLF —
            // the same thing any git checkout does under that config.)
            let readme = fs::read_to_string(repo.join("README.md")).unwrap();
            assert_eq!(readme.replace("\r\n", "\n"), "good agent work\n");
            assert!(repo.join("notes.txt").exists());

            let listed = cli.snapshot_list(&repo).expect("list");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].label, "before risky attempt");

            // An agent goes off the rails.
            fs::write(repo.join("README.md"), "ruined\n").expect("write");
            fs::remove_file(repo.join("notes.txt")).expect("rm");
            fs::write(repo.join("garbage.rs"), "half-finished\n").expect("write");

            cli.snapshot_restore(&repo, &listed[0].id).expect("restore");
            // Compare content, not line endings — autocrlf re-materializes
            // tracked files with CRLF on Windows, which is git config's business.
            let readme = fs::read_to_string(repo.join("README.md")).unwrap();
            assert_eq!(readme.replace("\r\n", "\n"), "good agent work\n");
            let notes = fs::read_to_string(repo.join("notes.txt")).unwrap();
            assert_eq!(notes.replace("\r\n", "\n"), "untracked but wanted\n");
            assert!(!repo.join("garbage.rs").exists(), "the mess is gone");

            // Kept after restore; dropping removes it.
            let listed = cli.snapshot_list(&repo).expect("list");
            assert_eq!(listed.len(), 1);
            cli.snapshot_drop(&repo, &listed[0].id).expect("drop");
            assert!(cli.snapshot_list(&repo).expect("list").is_empty());
        }

        #[test]
        fn commit_all_stages_untracked_files_too() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            fs::write(repo.join("tracked-edit.md"), "edited\n").expect("write");
            fs::write(repo.join("brand-new.txt"), "new\n").expect("write");
            git(&repo, &["add", "tracked-edit.md"]);

            let summary = cli
                .commit_all(&repo, "chore: commit everything")
                .expect("commit");
            assert!(summary.contains("chore: commit everything"), "{summary}");

            let status = cli.branch_status(&repo).expect("status");
            assert!(
                !status.dirty,
                "everything, including untracked, was committed"
            );

            // Nothing to commit is an error with git's own explanation, not a lie.
            let err = cli.commit_all(&repo, "empty").unwrap_err();
            assert!(err.to_string().contains("commit"), "{err}");
        }

        /// The Rider path end-to-end with real git: a branch that exists but is
        /// checked out nowhere; the primary switches to it and hosts the merge.
        #[test]
        fn switch_branch_then_merge_hosts_an_unchecked_out_target() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            // `develop` exists as a branch only — no worktree has it checked out.
            git(&repo, &["branch", "develop"]);

            let wt = tmp.path().join("wt");
            cli.create_worktree(&repo, &wt, "impl/T-1-x", Some("develop"))
                .expect("create worktree");
            fs::write(wt.join("feature.txt"), "x\n").expect("write");
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-m", "feature work"]);

            // What WorktreeManager::merge_into does for this case:
            cli.switch_branch(&repo, "develop").expect("switch");
            let head = GitCli::new()
                .run(&repo, &["symbolic-ref", "--short", "HEAD"])
                .expect("head");
            assert_eq!(head.trim(), "develop", "primary now hosts the target");

            let outcome = cli.merge_branch(&repo, "impl/T-1-x").expect("merge");
            assert!(outcome.merged, "{outcome:?}");
            assert!(
                repo.join("feature.txt").exists(),
                "the merged result is visible in the primary working tree"
            );
        }

        #[test]
        fn fresh_base_ref_is_a_no_op_without_a_remote() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);

            let cli = GitCli::new();
            assert_eq!(cli.fresh_base_ref(&repo, "main"), "main");
        }

        /// The exact bug report: a worktree's base branch moves on the remote after
        /// the worktree was created, and the diff must pick that up rather than
        /// stay frozen at whatever `develop` looked like at creation time.
        #[test]
        fn fresh_base_ref_fetches_before_returning_the_remote_tracking_ref() {
            let tmp = tempfile::tempdir().expect("tempdir");

            // A bare "origin" with a develop branch at commit A.
            let remote = tmp.path().join("remote.git");
            git(
                tmp.path(),
                &["init", "--bare", "-b", "develop", remote.to_str().unwrap()],
            );

            // A throwaway clone used only to push into the remote, standing in for
            // a teammate's machine.
            let seed = tmp.path().join("seed");
            git(
                tmp.path(),
                &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
            );
            git(&seed, &["config", "user.email", "test@maestro.local"]);
            git(&seed, &["config", "user.name", "Maestro Test"]);
            fs::write(seed.join("a.txt"), "a\n").expect("write");
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-m", "commit A"]);
            git(&seed, &["push", "origin", "develop"]);

            // The repo Maestro actually operates on: cloned once, then never
            // fetched again on its own — exactly how a worktree sits for however
            // long the user works on its branch.
            let repo = tmp.path().join("repo");
            git(
                tmp.path(),
                &["clone", remote.to_str().unwrap(), repo.to_str().unwrap()],
            );
            git(&repo, &["config", "user.email", "test@maestro.local"]);
            git(&repo, &["config", "user.name", "Maestro Test"]);

            let cli = GitCli::new();
            let wt = tmp.path().join("wt");
            cli.create_worktree(&repo, &wt, "impl/T-1-x", Some("develop"))
                .expect("create worktree");

            // Someone else advances develop on the remote after the worktree
            // exists — time passing, the scenario the bug report describes.
            fs::write(seed.join("b.txt"), "b\n").expect("write");
            git(&seed, &["add", "."]);
            git(&seed, &["commit", "-m", "commit B"]);
            git(&seed, &["push", "origin", "develop"]);

            // The repo's own local refs are still frozen at commit A: neither
            // `develop` nor `origin/develop` has moved without an explicit fetch.
            assert_eq!(
                cli.show_file(&repo, "develop", "b.txt").expect("show"),
                None,
                "local develop must still be stale before fetching"
            );

            let fresh = cli.fresh_base_ref(&repo, "develop");
            assert_eq!(
                fresh, "origin/develop",
                "resolves to the remote-tracking ref"
            );
            assert_eq!(
                cli.show_file(&repo, &fresh, "b.txt").expect("show"),
                Some("b\n".to_string()),
                "must have fetched — b.txt only exists after commit B"
            );

            // An already-remote-qualified base behaves the same way.
            assert_eq!(
                cli.fresh_base_ref(&repo, "origin/develop"),
                "origin/develop"
            );
        }

        /// The bug report: a file committed on `main` with LF gets the exact same
        /// content re-typed with CRLF on the branch (e.g. a Windows checkout with
        /// no `.gitattributes` to normalize it). Git still lists it as modified —
        /// the blob genuinely differs, `--name-status` is blob-hash based and does
        /// not consult whitespace-ignore flags — but the *diff content* (what the
        /// stats badge and the hunk view are built from) must come back empty
        /// instead of flagging every line, which is what `--ignore-cr-at-eol` is
        /// actually for.
        #[test]
        fn a_pure_crlf_rewrite_shows_modified_but_diffs_empty() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);
            // Deterministic regardless of the host machine's global git config:
            // with autocrlf on, `add`/`commit` would silently normalize the CRLF
            // rewrite below back to LF ("nothing to commit") before this test
            // ever gets to exercise the flag under test. Off is also the more
            // realistic case — a repo with no `.gitattributes` and a clone that
            // never had autocrlf configured, which is how these get committed at
            // all in the first place.
            git(&repo, &["config", "core.autocrlf", "false"]);
            fs::write(repo.join("service.cs"), "line one\nline two\nline three\n").expect("write");
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-m", "add service.cs"]);

            let cli = GitCli::new();
            let wt = tmp.path().join("wt-crlf");
            cli.create_worktree(&repo, &wt, "impl/T-crlf", Some("main"))
                .expect("create worktree");
            fs::write(
                wt.join("service.cs"),
                "line one\r\nline two\r\nline three\r\n",
            )
            .expect("rewrite with CRLF");
            git(&wt, &["add", "."]);
            git(&wt, &["commit", "-m", "same content, CRLF"]);

            let files = cli
                .changed_files(&repo, "impl/T-crlf", "main")
                .expect("changed files");
            assert_eq!(
                files.len(),
                1,
                "the blob really did change — git is right to list it: {files:?}"
            );
            assert_eq!(files[0].status, "M");
            let diff = cli
                .merge_base_diff(&repo, "impl/T-crlf", "main")
                .expect("diff");
            assert!(
                diff.trim().is_empty(),
                "the visible diff content must be empty despite the M: {diff}"
            );

            // Same story worktree-scope: `wt` already has the CRLF commit checked
            // out on disk, merge-base still has the original LF blob — the
            // on-disk-vs-blob case the original bug report actually was (a
            // Windows autocrlf checkout).
            let merge_base = cli.merge_base(&repo, "main", "impl/T-crlf").expect("mb");
            let wt_diff = cli.worktree_diff(&wt, &merge_base).expect("worktree diff");
            assert!(
                !wt_diff.contains("service.cs"),
                "worktree diff text must not mention it either: {wt_diff}"
            );
        }

        /// The regression this guards against: concurrent git invocations against
        /// the same worktree used to race on `.git/index` — `git status` refreshes
        /// the index as a side effect despite being nominally read-only, and could
        /// collide with a genuinely mutating command (e.g. `stash push` during a
        /// merge) running at the same moment, failing with "could not write index".
        /// `GitCli::output` now serializes every invocation behind `exec_lock`; this
        /// hammers one worktree from many threads at once — status polls racing a
        /// park/restore cycle, exactly the shapes `WorktreeManager` fires
        /// concurrently in practice — and asserts none of them ever fail.
        #[test]
        fn concurrent_git_invocations_never_race_on_the_index() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = tmp.path().join("repo");
            fs::create_dir(&repo).expect("mkdir");
            init_repo(&repo);
            fs::write(repo.join("scratch.txt"), "start\n").expect("write");
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-m", "add scratch.txt"]);

            let cli = std::sync::Arc::new(GitCli::new());
            let mut handles = Vec::new();

            // Status-poll threads: what the frontend's periodic refresh does.
            for _ in 0..6 {
                let cli = cli.clone();
                let repo = repo.clone();
                handles.push(std::thread::spawn(move || {
                    for _ in 0..20 {
                        cli.branch_status(&repo).expect("concurrent branch_status");
                    }
                }));
            }

            // A park/restore thread: the merge_into "park dirty target" step,
            // running at the same time as the status polls above.
            {
                let cli = cli.clone();
                let repo = repo.clone();
                handles.push(std::thread::spawn(move || {
                    for i in 0..10 {
                        fs::write(repo.join("scratch.txt"), format!("edit {i}\n")).expect("write");
                        cli.snapshot_push(&repo, &format!("pre-merge {i}"))
                            .expect("concurrent snapshot_push");
                        cli.discard_changes(&repo)
                            .expect("concurrent discard_changes");
                        let listed = cli.snapshot_list(&repo).expect("concurrent snapshot_list");
                        cli.snapshot_restore(&repo, &listed[0].id)
                            .expect("concurrent snapshot_restore");
                        cli.snapshot_drop(&repo, &listed[0].id)
                            .expect("concurrent snapshot_drop");
                    }
                }));
            }

            for handle in handles {
                handle.join().expect("thread panicked");
            }
        }
    }
}
