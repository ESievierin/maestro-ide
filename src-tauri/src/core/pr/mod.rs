//! User-initiated PR actions: create a pull request, list its review
//! comments, post replies. This is the *human's* half of the review loop — the
//! daemon only ever prepares text; everything in this module runs because a
//! button was clicked, which is the HITL guarantee.
//!
//! All GitHub calls act as the account picked in the daemon panel (per-call
//! `GH_TOKEN`); gh's globally active account is never switched.

use std::sync::Arc;

use serde::Serialize;

use crate::core::bus::EventBus;
use crate::core::daemon::{resolve_account, resolve_slug, GhProvider, NewReviewComment};
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// What `create` did, for the dialog's success view.
#[derive(Clone, Debug, Serialize)]
pub struct CreatedPr {
    pub url: String,
    pub push_report: String,
}

/// One review comment of the branch's open PR.
#[derive(Clone, Debug, Serialize)]
pub struct PrComment {
    pub pr: u64,
    pub id: u64,
    pub author: String,
    pub path: String,
    pub body: String,
    pub url: String,
}

/// Result of posting one reply.
#[derive(Clone, Debug, Serialize)]
pub struct ReplyOutcome {
    pub comment_id: u64,
    pub ok: bool,
    /// The reply's URL on success, the error message on failure.
    pub detail: String,
}

/// One comment a human approved for posting — either a reply to a comment
/// that already exists (`in_reply_to` set) or a brand-new one anchored to a
/// file+line. What the review-comments dialog hands `post_review_comments`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct DraftComment {
    pub path: String,
    pub line: u64,
    pub side: Option<String>,
    pub body: String,
    pub in_reply_to: Option<u64>,
}

/// Result of `post_review_comments`. New comments are grouped into one
/// review submission (GitHub's own grouping), so a failure there is one
/// entry covering all of them, not one per comment.
#[derive(Clone, Debug, Serialize)]
pub struct PostCommentsOutcome {
    pub posted: usize,
    pub failed: Vec<String>,
}

pub struct PrManager {
    store: Arc<dyn Store>,
    gh: Arc<dyn GhProvider>,
    worktrees: Arc<WorktreeManager>,
    #[allow(dead_code)]
    bus: EventBus,
}

impl PrManager {
    pub fn new(
        store: Arc<dyn Store>,
        gh: Arc<dyn GhProvider>,
        worktrees: Arc<WorktreeManager>,
        bus: EventBus,
    ) -> Self {
        Self {
            store,
            gh,
            worktrees,
            bus,
        }
    }

    fn token(&self) -> Result<(String, String)> {
        let accounts = self.gh.accounts()?;
        let account = resolve_account(self.store.as_ref(), &accounts);
        if account.is_empty() {
            return Err(MaestroError::Config {
                message: "no gh account available — log in with `gh auth login`".into(),
            });
        }
        let token = self.gh.token(&account)?;
        Ok((account, token))
    }

    fn slug(&self) -> Result<String> {
        let repo = self.worktrees.repo_info()?.map(|r| r.path);
        resolve_slug(self.store.as_ref(), repo.as_deref())
    }

    /// The base to open the PR against: an explicit override, else the
    /// branch's stored base, else the repository's default branch. A
    /// remote-tracking name ("origin/main") is reduced to its short form —
    /// PRs target branch names, not refs.
    fn resolve_pr_base(&self, branch: &str, base: Option<&str>) -> Result<String> {
        let base = match base.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => base.to_string(),
            None => match self.store.get_branch(branch)?.and_then(|b| b.base_branch) {
                Some(base) => base,
                None => {
                    self.worktrees
                        .repo_info()?
                        .ok_or_else(|| MaestroError::Config {
                            message: "no repository selected".into(),
                        })?
                        .default_branch
                }
            },
        };
        Ok(base
            .rsplit_once('/')
            .map_or(base.as_str(), |(maybe_remote, short)| {
                if maybe_remote == "origin" {
                    short
                } else {
                    base.as_str()
                }
            })
            .to_string())
    }

    /// Push `branch` and open a PR for it. The branch must be committed; the
    /// dialog handles the commit step first. `base` overrides the stored/
    /// default base when the user picked one explicitly.
    pub fn create(
        &self,
        branch: &str,
        title: &str,
        body: &str,
        base: Option<&str>,
    ) -> Result<CreatedPr> {
        let title = title.trim();
        if title.is_empty() {
            return Err(MaestroError::InvalidData {
                message: "a PR needs a title".into(),
            });
        }
        let info = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        let base = self.resolve_pr_base(branch, base)?;

        let push_report = self.worktrees.push(branch)?;
        let (_, token) = self.token()?;
        let url = self
            .gh
            .create_pr(&token, &info.path, &base, branch, title, body)?;
        tracing::info!(branch, %url, "pull request created");
        Ok(CreatedPr { url, push_report })
    }

    /// Review comments of the open PR whose head is `branch` (empty when the
    /// branch has no open PR). Comments whose thread is already resolved on
    /// GitHub are left out — there is nothing to reply to on a thread the
    /// author or reviewer already marked done.
    pub fn comments(&self, branch: &str) -> Result<Vec<PrComment>> {
        let (_, token) = self.token()?;
        let slug = self.slug()?;
        let Some(pull) = self
            .gh
            .open_pulls(&token, &slug)?
            .into_iter()
            .find(|p| p.head_ref == branch)
        else {
            return Ok(Vec::new());
        };
        let resolved = self
            .gh
            .resolved_comment_ids(&token, &slug, pull.number)
            .unwrap_or_default();
        Ok(self
            .gh
            .pull_comments(&token, &slug, pull.number)?
            .into_iter()
            .filter(|c| !resolved.contains(&c.id))
            .map(|c| PrComment {
                pr: pull.number,
                id: c.id,
                author: c.author,
                path: c.path.unwrap_or_default(),
                body: c.body,
                url: c.url,
            })
            .collect())
    }

    /// Post replies, one per comment. Each reply stands alone: a failure on
    /// one does not roll back the others — the outcome list says exactly what
    /// landed.
    pub fn reply(&self, pr: u64, replies: &[(u64, String)]) -> Result<Vec<ReplyOutcome>> {
        let (_, token) = self.token()?;
        let slug = self.slug()?;
        let mut outcomes = Vec::with_capacity(replies.len());
        for (comment_id, body) in replies {
            if body.trim().is_empty() {
                continue;
            }
            match self
                .gh
                .reply_to_comment(&token, &slug, pr, *comment_id, body.trim())
            {
                Ok(url) => outcomes.push(ReplyOutcome {
                    comment_id: *comment_id,
                    ok: true,
                    detail: url,
                }),
                Err(err) => outcomes.push(ReplyOutcome {
                    comment_id: *comment_id,
                    ok: false,
                    detail: err.to_string(),
                }),
            }
        }
        tracing::info!(
            pr,
            posted = outcomes.iter().filter(|o| o.ok).count(),
            failed = outcomes.iter().filter(|o| !o.ok).count(),
            "PR replies posted"
        );
        Ok(outcomes)
    }

    /// Post a human-approved batch of draft comments — the review-comments
    /// dialog's "Approve" action. Replies (an existing comment id) each go
    /// through the reply endpoint individually, exactly like `reply()`;
    /// brand-new comments are grouped into one review submission, since
    /// that is how GitHub itself groups "here is a batch of findings" —
    /// one review, many comments, one place to see them all together.
    pub fn post_review_comments(
        &self,
        pr: u64,
        drafts: &[DraftComment],
    ) -> Result<PostCommentsOutcome> {
        let (_, token) = self.token()?;
        let slug = self.slug()?;
        let mut posted = 0usize;
        let mut failed = Vec::new();

        for draft in drafts.iter().filter(|d| d.in_reply_to.is_some()) {
            let body = draft.body.trim();
            if body.is_empty() {
                continue;
            }
            let comment_id = draft.in_reply_to.expect("filtered to Some above");
            match self
                .gh
                .reply_to_comment(&token, &slug, pr, comment_id, body)
            {
                Ok(_) => posted += 1,
                Err(err) => failed.push(format!("reply to comment {comment_id}: {err}")),
            }
        }

        let new_comments: Vec<NewReviewComment> = drafts
            .iter()
            .filter(|d| d.in_reply_to.is_none() && !d.body.trim().is_empty())
            .map(|d| NewReviewComment {
                path: d.path.clone(),
                line: d.line,
                side: d.side.clone(),
                body: d.body.trim().to_string(),
            })
            .collect();
        if !new_comments.is_empty() {
            let count = new_comments.len();
            match self.gh.create_review(&token, &slug, pr, &new_comments) {
                Ok(_) => posted += count,
                Err(err) => failed.push(format!("review with {count} new comment(s): {err}")),
            }
        }

        tracing::info!(pr, posted, failed = failed.len(), "review comments posted");
        Ok(PostCommentsOutcome { posted, failed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::daemon::github::{GhComment, GhPull};
    use crate::core::daemon::{GhAccount, SETTING_DAEMON_REPO};
    use crate::core::store::SqliteStore;
    use crate::core::worktree::{CreateWorktreeRequest, GitCli, WorktreeManager};
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    #[derive(Default)]
    struct StubGh {
        pulls: Vec<GhPull>,
        comments: Vec<GhComment>,
        resolved: Vec<u64>,
        reply_fails: bool,
        review_fails: bool,
    }
    impl GhProvider for StubGh {
        fn accounts(&self) -> Result<Vec<GhAccount>> {
            Ok(vec![GhAccount {
                login: "me".into(),
                active: true,
            }])
        }
        fn token(&self, _account: &str) -> Result<String> {
            Ok("tok".into())
        }
        fn open_pulls(&self, _t: &str, _s: &str) -> Result<Vec<GhPull>> {
            Ok(self.pulls.clone())
        }
        fn pull_comments(&self, _t: &str, _s: &str, _n: u64) -> Result<Vec<GhComment>> {
            Ok(self.comments.clone())
        }
        fn resolved_comment_ids(
            &self,
            _t: &str,
            _s: &str,
            _n: u64,
        ) -> Result<std::collections::HashSet<u64>> {
            Ok(self.resolved.iter().copied().collect())
        }
        fn reply_to_comment(
            &self,
            _t: &str,
            _s: &str,
            _pr: u64,
            comment_id: u64,
            _body: &str,
        ) -> Result<String> {
            if self.reply_fails {
                return Err(MaestroError::Config {
                    message: "reply failed (stub)".into(),
                });
            }
            Ok(format!(
                "https://github.com/owner/repo/pull/42#discussion_r{comment_id}"
            ))
        }
        fn create_review(
            &self,
            _t: &str,
            _s: &str,
            _pr: u64,
            _comments: &[crate::core::daemon::NewReviewComment],
        ) -> Result<String> {
            if self.review_fails {
                return Err(MaestroError::Config {
                    message: "review failed (stub)".into(),
                });
            }
            Ok("https://github.com/owner/repo/pull/42#pullrequestreview-1".into())
        }
    }

    fn git_cmd(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn setup() -> (PrManager, String, tempfile::TempDir) {
        setup_with_gh(|_branch| StubGh::default())
    }

    /// `build_gh` sees the freshly created branch name, so a test can point a
    /// `GhPull.head_ref` at it before the `PrManager` is built.
    fn setup_with_gh(
        build_gh: impl FnOnce(&str) -> StubGh,
    ) -> (PrManager, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git_cmd(&repo, &["init", "-b", "main"]);
        git_cmd(&repo, &["config", "user.email", "t@t.t"]);
        git_cmd(&repo, &["config", "user.name", "t"]);
        fs::write(repo.join("a.txt"), "base\n").unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "init"]);

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
                slug: Some("pr test".into()),
                base: Some("main".into()),
            })
            .unwrap();
        let branch = info.branch.unwrap();

        store
            .set_setting(SETTING_DAEMON_REPO, "owner/repo")
            .unwrap();
        let gh = build_gh(&branch);
        let prs = PrManager::new(store, Arc::new(gh), worktrees, EventBus::new());
        (prs, branch, tmp)
    }

    #[test]
    fn an_explicit_base_wins_over_everything_stored() {
        let (prs, branch, _tmp) = setup();
        assert_eq!(
            prs.resolve_pr_base(&branch, Some("develop")).unwrap(),
            "develop"
        );
    }

    #[test]
    fn it_falls_back_to_the_branch_s_stored_base() {
        let (prs, branch, _tmp) = setup();
        // `setup` created the worktree with base "main" — the store row
        // already carries that as `base_branch`.
        assert_eq!(prs.resolve_pr_base(&branch, None).unwrap(), "main");
    }

    #[test]
    fn a_blank_override_is_treated_as_absent() {
        let (prs, branch, _tmp) = setup();
        assert_eq!(prs.resolve_pr_base(&branch, Some("   ")).unwrap(), "main");
    }

    #[test]
    fn a_remote_tracking_override_is_reduced_to_its_short_name() {
        let (prs, branch, _tmp) = setup();
        assert_eq!(
            prs.resolve_pr_base(&branch, Some("origin/develop"))
                .unwrap(),
            "develop"
        );
    }

    #[test]
    fn create_refuses_an_empty_title() {
        let (prs, branch, _tmp) = setup();
        let err = prs.create(&branch, "  ", "body", None).unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    fn pull(head_ref: &str) -> GhPull {
        GhPull {
            number: 42,
            title: "Add retry".into(),
            body: "PR body".into(),
            author: "colleague".into(),
            head_ref: head_ref.into(),
            head_sha: "sha-1".into(),
            url: "https://github.com/owner/repo/pull/42".into(),
            requested_reviewers: Vec::new(),
            labels: Vec::new(),
        }
    }

    fn comment(id: u64) -> GhComment {
        GhComment {
            id,
            body: format!("Comment {id}"),
            author: "reviewer".into(),
            path: Some("src/retry.rs".into()),
            url: format!("https://github.com/owner/repo/pull/42#discussion_r{id}"),
            review_id: Some(900),
        }
    }

    #[test]
    fn comments_already_resolved_on_github_are_left_out() {
        let (prs, branch, _tmp) = setup_with_gh(|branch| StubGh {
            pulls: vec![pull(branch)],
            comments: vec![comment(501), comment(502)],
            resolved: vec![501],
            ..Default::default()
        });
        let comments = prs.comments(&branch).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, 502);
    }

    #[test]
    fn comments_with_nothing_resolved_all_come_through() {
        let (prs, branch, _tmp) = setup_with_gh(|branch| StubGh {
            pulls: vec![pull(branch)],
            comments: vec![comment(501), comment(502)],
            resolved: Vec::new(),
            ..Default::default()
        });
        let comments = prs.comments(&branch).unwrap();
        assert_eq!(comments.len(), 2);
    }

    fn draft_reply(comment_id: u64, body: &str) -> DraftComment {
        DraftComment {
            path: String::new(),
            line: 0,
            side: None,
            body: body.into(),
            in_reply_to: Some(comment_id),
        }
    }

    fn draft_new(path: &str, line: u64, body: &str) -> DraftComment {
        DraftComment {
            path: path.into(),
            line,
            side: None,
            body: body.into(),
            in_reply_to: None,
        }
    }

    #[test]
    fn post_review_comments_posts_replies_and_a_grouped_review_together() {
        let (prs, _branch, _tmp) = setup();
        let drafts = vec![
            draft_reply(501, "Good catch, fixed."),
            draft_new("src/lib.rs", 42, "This could overflow on a large input."),
            draft_new("src/lib.rs", 50, "Missing a null check here."),
        ];
        let outcome = prs.post_review_comments(42, &drafts).unwrap();
        assert_eq!(outcome.posted, 3, "1 reply + 2 new comments in one review");
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn a_failed_reply_does_not_block_the_grouped_review() {
        let (prs, _branch, _tmp) = setup_with_gh(|_| StubGh {
            reply_fails: true,
            ..Default::default()
        });
        let drafts = vec![
            draft_reply(501, "This will fail."),
            draft_new("src/lib.rs", 42, "A brand-new finding."),
        ];
        let outcome = prs.post_review_comments(42, &drafts).unwrap();
        assert_eq!(outcome.posted, 1, "the review still went through");
        assert_eq!(outcome.failed.len(), 1);
        assert!(outcome.failed[0].contains("501"), "{:?}", outcome.failed);
    }

    #[test]
    fn a_failed_review_does_not_block_already_posted_replies() {
        let (prs, _branch, _tmp) = setup_with_gh(|_| StubGh {
            review_fails: true,
            ..Default::default()
        });
        let drafts = vec![
            draft_reply(501, "Posted fine."),
            draft_new("src/lib.rs", 42, "This review submission fails."),
        ];
        let outcome = prs.post_review_comments(42, &drafts).unwrap();
        assert_eq!(outcome.posted, 1, "the reply still landed");
        assert_eq!(outcome.failed.len(), 1);
        assert!(
            outcome.failed[0].contains("1 new comment"),
            "{:?}",
            outcome.failed
        );
    }

    #[test]
    fn blank_drafts_are_skipped_and_never_counted() {
        let (prs, _branch, _tmp) = setup();
        let drafts = vec![draft_reply(501, "   "), draft_new("src/lib.rs", 1, "")];
        let outcome = prs.post_review_comments(42, &drafts).unwrap();
        assert_eq!(outcome.posted, 0);
        assert!(outcome.failed.is_empty());
    }
}
