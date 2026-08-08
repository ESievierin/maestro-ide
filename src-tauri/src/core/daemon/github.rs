//! GitHub boundary for the daemon, over the `gh` CLI.
//!
//! Account handling is deliberate: `gh` holds several logins (personal and
//! work), and the daemon must never depend on which one happens to be
//! globally active. Every API call carries the token of the *configured*
//! account (`gh auth token --user <login>` → `GH_TOKEN`), so the user's own
//! `gh` usage and the daemon never fight over the active-account switch.

use std::process::{Command, Stdio};

use crate::error::{GitErrorKind, MaestroError, Result};

/// One `gh` login known on this machine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GhAccount {
    pub login: String,
    /// Whether this is gh's globally active account (shown as the default pick).
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct GhPull {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub head_ref: String,
    pub head_sha: String,
    pub url: String,
    /// Logins whose review is currently requested on this PR.
    pub requested_reviewers: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct GhComment {
    pub id: u64,
    pub body: String,
    pub author: String,
    pub path: Option<String>,
    pub url: String,
    /// The review submission this comment belongs to — comments left in the same
    /// review share one id, which is what "group by review" groups on. `None` for
    /// the rare comment GitHub didn't attach to any review.
    pub review_id: Option<u64>,
}

/// External GitHub boundary; test doubles implement this.
pub trait GhProvider: Send + Sync {
    /// Logins `gh` knows about, with the globally active one flagged.
    fn accounts(&self) -> Result<Vec<GhAccount>>;
    /// The token for `account` — the guard that the daemon acts as who it
    /// should: no token, no polling.
    fn token(&self, account: &str) -> Result<String>;
    /// Open pull requests in `slug`.
    fn open_pulls(&self, token: &str, slug: &str) -> Result<Vec<GhPull>>;
    /// Review comments of one pull request.
    fn pull_comments(&self, token: &str, slug: &str, number: u64) -> Result<Vec<GhComment>>;

    /// Ids of comments whose review thread is marked resolved on GitHub. The plain
    /// REST comments endpoint has no such field — only GraphQL's `PullRequestReviewThread`
    /// does — so this is a separate, best-effort lookup: callers treat a failure as
    /// "nothing resolved" rather than let it block listing or replying to comments.
    fn resolved_comment_ids(
        &self,
        _token: &str,
        _slug: &str,
        _number: u64,
    ) -> Result<std::collections::HashSet<u64>> {
        Ok(std::collections::HashSet::new())
    }

    /// `gh pr create` in `cwd` — returns the new PR's URL. User-initiated only;
    /// the daemon never calls this.
    fn create_pr(
        &self,
        _token: &str,
        _cwd: &std::path::Path,
        _base: &str,
        _head: &str,
        _title: &str,
        _body: &str,
    ) -> Result<String> {
        Err(MaestroError::Config {
            message: "create_pr is not supported by this provider".into(),
        })
    }

    /// Reply to a PR review comment — returns the reply's URL. User-initiated
    /// only; the daemon never calls this.
    fn reply_to_comment(
        &self,
        _token: &str,
        _slug: &str,
        _pr: u64,
        _comment_id: u64,
        _body: &str,
    ) -> Result<String> {
        Err(MaestroError::Config {
            message: "reply_to_comment is not supported by this provider".into(),
        })
    }
}

pub struct GhCli;

impl GhCli {
    fn run(&self, args: &[&str], token: Option<&str>) -> Result<String> {
        self.run_in(args, token, None)
    }

    fn run_in(
        &self,
        args: &[&str],
        token: Option<&str>,
        cwd: Option<&std::path::Path>,
    ) -> Result<String> {
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if let Some(token) = token {
            cmd.env("GH_TOKEN", token);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = cmd.output().map_err(|err| MaestroError::Git {
            kind: GitErrorKind::NotInstalled,
            message: format!("failed to launch gh: {err}"),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MaestroError::Git {
                kind: GitErrorKind::CommandFailed,
                message: format!("`gh {}` failed: {}", args.join(" "), stderr.trim()),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl GhProvider for GhCli {
    fn accounts(&self) -> Result<Vec<GhAccount>> {
        // `gh auth status` has no JSON output; its text format is stable enough:
        //   ✓ Logged in to github.com account NAME (keyring)
        //   - Active account: true
        let out = self.run(&["auth", "status"], None)?;
        Ok(parse_auth_status(&out))
    }

    fn token(&self, account: &str) -> Result<String> {
        let out = self.run(&["auth", "token", "--user", account], None)?;
        let token = out.trim().to_string();
        if token.is_empty() {
            return Err(MaestroError::Config {
                message: format!("gh returned no token for account '{account}'"),
            });
        }
        Ok(token)
    }

    fn open_pulls(&self, token: &str, slug: &str) -> Result<Vec<GhPull>> {
        let path = format!("repos/{slug}/pulls?state=open&per_page=50");
        let out = self.run(&["api", &path], Some(token))?;
        let raw: Vec<serde_json::Value> = serde_json::from_str(&out).map_err(bad_json)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| {
                Some(GhPull {
                    number: v.get("number")?.as_u64()?,
                    title: v.get("title")?.as_str()?.to_string(),
                    body: v
                        .get("body")
                        .and_then(|b| b.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    author: v
                        .get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|l| l.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    head_ref: v.get("head")?.get("ref")?.as_str()?.to_string(),
                    head_sha: v
                        .get("head")
                        .and_then(|h| h.get("sha"))
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: v
                        .get("html_url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    requested_reviewers: v
                        .get("requested_reviewers")
                        .and_then(|r| r.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|u| u.get("login").and_then(|l| l.as_str()))
                                .map(|l| l.to_string())
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    fn pull_comments(&self, token: &str, slug: &str, number: u64) -> Result<Vec<GhComment>> {
        let path = format!("repos/{slug}/pulls/{number}/comments?per_page=100");
        let out = self.run(&["api", &path], Some(token))?;
        let raw: Vec<serde_json::Value> = serde_json::from_str(&out).map_err(bad_json)?;
        Ok(raw
            .into_iter()
            .filter_map(|v| {
                Some(GhComment {
                    id: v.get("id")?.as_u64()?,
                    body: v.get("body")?.as_str()?.to_string(),
                    author: v
                        .get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|l| l.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    path: v
                        .get("path")
                        .and_then(|p| p.as_str())
                        .map(|p| p.to_string()),
                    url: v
                        .get("html_url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    review_id: v.get("pull_request_review_id").and_then(|r| r.as_u64()),
                })
            })
            .collect())
    }

    fn resolved_comment_ids(
        &self,
        token: &str,
        slug: &str,
        number: u64,
    ) -> Result<std::collections::HashSet<u64>> {
        let (owner, name) = slug
            .split_once('/')
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("malformed repo slug: {slug}"),
            })?;
        let query = "query($owner: String!, $name: String!, $number: Int!) { \
            repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
            reviewThreads(first: 100) { nodes { isResolved comments(first: 100) { \
            nodes { databaseId } } } } } } }";
        let query_arg = format!("query={query}");
        let owner_arg = format!("owner={owner}");
        let name_arg = format!("name={name}");
        let number_arg = format!("number={number}");
        let out = self.run(
            &[
                "api",
                "graphql",
                "-f",
                &query_arg,
                "-f",
                &owner_arg,
                "-f",
                &name_arg,
                "-F",
                &number_arg,
            ],
            Some(token),
        )?;
        parse_resolved_ids(&out)
    }

    fn create_pr(
        &self,
        token: &str,
        cwd: &std::path::Path,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let out = self.run_in(
            &[
                "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body,
            ],
            Some(token),
            Some(cwd),
        )?;
        // gh prints the PR URL as the last non-empty stdout line.
        Ok(out
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string())
    }

    fn reply_to_comment(
        &self,
        token: &str,
        slug: &str,
        pr: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<String> {
        let path = format!("repos/{slug}/pulls/{pr}/comments/{comment_id}/replies");
        let out = self.run(
            &["api", "-X", "POST", &path, "-f", &format!("body={body}")],
            Some(token),
        )?;
        let parsed: serde_json::Value = serde_json::from_str(&out).map_err(bad_json)?;
        Ok(parsed
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

fn bad_json(err: serde_json::Error) -> MaestroError {
    MaestroError::InvalidData {
        message: format!("unexpected gh api response: {err}"),
    }
}

/// Pull the REST comment ids out of every resolved thread in a
/// `reviewThreads` GraphQL response. Tolerant of a missing/malformed shape —
/// an unexpected response yields no resolved ids rather than an error, since
/// callers already treat this lookup as best-effort.
fn parse_resolved_ids(raw: &str) -> Result<std::collections::HashSet<u64>> {
    let parsed: serde_json::Value = serde_json::from_str(raw).map_err(bad_json)?;
    let nodes = parsed
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut resolved = std::collections::HashSet::new();
    for thread in nodes {
        if thread.get("isResolved").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let Some(comments) = thread.pointer("/comments/nodes").and_then(|n| n.as_array()) else {
            continue;
        };
        for comment in comments {
            if let Some(id) = comment.get("databaseId").and_then(|v| v.as_u64()) {
                resolved.insert(id);
            }
        }
    }
    Ok(resolved)
}

/// Parse `gh auth status` text into accounts. Tolerant: unknown lines are skipped.
fn parse_auth_status(out: &str) -> Vec<GhAccount> {
    let mut accounts: Vec<GhAccount> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if let Some(idx) = line.find(" account ") {
            let rest = &line[idx + " account ".len()..];
            let login = rest.split_whitespace().next().unwrap_or("").to_string();
            if !login.is_empty() {
                accounts.push(GhAccount {
                    login,
                    active: false,
                });
            }
        } else if line.contains("Active account: true") {
            if let Some(last) = accounts.last_mut() {
                last.active = true;
            }
        }
    }
    accounts
}

/// `owner/name` from a git remote URL — SSH aliases, ssh:// and https forms all
/// reduce to the last two path segments.
pub fn slug_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git");
    // host:owner/name  |  ssh://git@host/owner/name  |  https://host/owner/name
    let path_part = if let Some((_, after_colon)) = trimmed.rsplit_once(':') {
        // "C:\..." is a Windows path, not an SSH remote — reject drive letters.
        if after_colon.starts_with('\\') || after_colon.starts_with('/') && trimmed.len() < 4 {
            return None;
        }
        after_colon
    } else {
        trimmed
    };
    let segments: Vec<&str> = path_part
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[segments.len() - 2];
    let name = segments[segments.len() - 1];
    if owner.contains('.') && segments.len() >= 3 {
        // https://github.com/owner/name → segments are [github.com, owner, name]
        return Some(format!("{}/{}", segments[segments.len() - 2], name));
    }
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auth_status_with_active_flag() {
        let out = "github.com\n\
                   ✓ Logged in to github.com account ESievierin (keyring)\n\
                   - Active account: true\n\
                   - Git operations protocol: ssh\n\
                   \n\
                   ✓ Logged in to github.com account Yehor-Sievierin-Rply (keyring)\n\
                   - Active account: false\n";
        let accounts = parse_auth_status(out);
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].login, "ESievierin");
        assert!(accounts[0].active);
        assert_eq!(accounts[1].login, "Yehor-Sievierin-Rply");
        assert!(!accounts[1].active);
    }

    #[test]
    fn resolved_ids_come_only_from_resolved_threads() {
        let raw = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "isResolved": true,
                                    "comments": { "nodes": [{ "databaseId": 111 }, { "databaseId": 112 }] }
                                },
                                {
                                    "isResolved": false,
                                    "comments": { "nodes": [{ "databaseId": 222 }] }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let resolved = parse_resolved_ids(raw).expect("parses");
        assert_eq!(resolved, [111, 112].into_iter().collect());
    }

    #[test]
    fn resolved_ids_is_empty_on_an_unexpected_shape() {
        let resolved = parse_resolved_ids(r#"{"data": null}"#).expect("still parses as JSON");
        assert!(resolved.is_empty());
    }

    #[test]
    fn slugs_from_all_remote_url_shapes() {
        assert_eq!(
            slug_from_remote_url("git@github.com:reply-team/replyapp.git").as_deref(),
            Some("reply-team/replyapp")
        );
        assert_eq!(
            slug_from_remote_url("github-maestro:ESievierin/maestro-ide.git").as_deref(),
            Some("ESievierin/maestro-ide")
        );
        assert_eq!(
            slug_from_remote_url("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            slug_from_remote_url("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(slug_from_remote_url("not-a-remote"), None);
    }
}
