//! Jira boundary for the daemon, over `curl` (bundled with Windows 10+).
//!
//! Off unless `jira_base_url` + `jira_email` + `jira_token` are configured.
//! Credentials travel to curl via stdin (`--config -`), never on the command
//! line where any process viewer could read them.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::{MaestroError, Result};

/// Base URL of the Jira site, e.g. `https://yourorg.atlassian.net`.
pub const SETTING_JIRA_BASE_URL: &str = "jira_base_url";
/// The Atlassian account email the API token belongs to.
pub const SETTING_JIRA_EMAIL: &str = "jira_email";
/// An Atlassian API token (id.atlassian.com → Security → API tokens).
pub const SETTING_JIRA_TOKEN: &str = "jira_token";
/// Optional JQL override for what "my work" means.
pub const SETTING_JIRA_JQL: &str = "jira_jql";

pub const DEFAULT_JQL: &str =
    "assignee = currentUser() AND resolution = EMPTY AND statusCategory != Done ORDER BY updated DESC";

#[derive(Clone, Debug)]
pub struct JiraConfig {
    pub base_url: String,
    pub email: String,
    pub token: String,
    pub jql: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JiraIssue {
    pub key: String,
    pub summary: String,
    pub description: String,
    pub url: String,
}

/// External Jira boundary; test doubles implement this.
pub trait JiraProvider: Send + Sync {
    /// Issues matching the configured JQL, newest-updated first.
    fn my_issues(&self, config: &JiraConfig) -> Result<Vec<JiraIssue>>;
}

pub struct JiraCli;

impl JiraProvider for JiraCli {
    fn my_issues(&self, config: &JiraConfig) -> Result<Vec<JiraIssue>> {
        let base = config.base_url.trim_end_matches('/');
        let url = format!(
            "{base}/rest/api/2/search?maxResults=50&fields=summary,description&jql={}",
            urlencode(&config.jql)
        );
        let out = curl_json(&url, &config.email, &config.token)?;
        let parsed: serde_json::Value =
            serde_json::from_str(&out).map_err(|err| MaestroError::InvalidData {
                message: format!("unexpected Jira response: {err}"),
            })?;
        if let Some(messages) = parsed.get("errorMessages").and_then(|m| m.as_array()) {
            let joined = messages
                .iter()
                .filter_map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(MaestroError::Config {
                message: format!("Jira rejected the query: {joined}"),
            });
        }
        let issues = parsed
            .get("issues")
            .and_then(|i| i.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let key = v.get("key")?.as_str()?.to_string();
                        let fields = v.get("fields")?;
                        Some(JiraIssue {
                            url: format!("{base}/browse/{key}"),
                            summary: fields
                                .get("summary")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string(),
                            description: fields
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .chars()
                                .take(4000)
                                .collect(),
                            key,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(issues)
    }
}

/// GET `url` with basic auth, credentials via stdin config. 20s timeout so a
/// hung proxy cannot stall the daemon loop.
fn curl_json(url: &str, email: &str, token: &str) -> Result<String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--fail-with-body",
        "--max-time",
        "20",
        "-H",
        "Accept: application/json",
        "--config",
        "-",
        url,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|err| MaestroError::Config {
        message: format!("failed to launch curl: {err}"),
    })?;
    // `user = "email:token"` — quoted so special characters survive.
    let config_line = format!(
        "user = \"{}:{}\"\n",
        email.replace('"', ""),
        token.replace('"', "")
    );
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(config_line.as_bytes())
            .map_err(|err| MaestroError::Config {
                message: format!("could not pass credentials to curl: {err}"),
            })?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|err| MaestroError::Config {
            message: format!("curl did not finish: {err}"),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 4xx bodies land on stdout thanks to --fail-with-body; prefer them,
        // they carry Jira's actual error message.
        let detail = if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().chars().take(300).collect()
        };
        return Err(MaestroError::Config {
            message: format!("Jira request failed: {detail}"),
        });
    }
    Ok(stdout)
}

/// Minimal percent-encoding for a JQL query-string value.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jql_is_percent_encoded() {
        assert_eq!(urlencode("a = b"), "a%20%3D%20b");
        assert_eq!(urlencode("safe-chars_only.~"), "safe-chars_only.~");
    }
}
