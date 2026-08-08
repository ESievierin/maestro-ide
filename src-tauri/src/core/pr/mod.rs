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
use crate::core::daemon::{resolve_account, resolve_slug, GhProvider};
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

    /// Push `branch` and open a PR for it. The branch must be committed; the
    /// dialog handles the commit step first.
    pub fn create(&self, branch: &str, title: &str, body: &str) -> Result<CreatedPr> {
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
        let base = match self.store.get_branch(branch)?.and_then(|b| b.base_branch) {
            Some(base) => base,
            None => {
                self.worktrees
                    .repo_info()?
                    .ok_or_else(|| MaestroError::Config {
                        message: "no repository selected".into(),
                    })?
                    .default_branch
            }
        };
        // Base may be a remote-tracking name ("origin/main") — PRs target the
        // short branch name.
        let base = base
            .rsplit_once('/')
            .map_or(base.as_str(), |(maybe_remote, short)| {
                if maybe_remote == "origin" {
                    short
                } else {
                    base.as_str()
                }
            })
            .to_string();

        let push_report = self.worktrees.push(branch)?;
        let (_, token) = self.token()?;
        let url = self
            .gh
            .create_pr(&token, &info.path, &base, branch, title, body)?;
        tracing::info!(branch, %url, "pull request created");
        Ok(CreatedPr { url, push_report })
    }

    /// Review comments of the open PR whose head is `branch` (empty when the
    /// branch has no open PR).
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
        Ok(self
            .gh
            .pull_comments(&token, &slug, pull.number)?
            .into_iter()
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
}
