//! Prompt template engine (T6 + T8).
//!
//! Prompts are data: markdown files with frontmatter in `~/.maestro/prompts/`, rendered
//! through `{{var}}` substitution. New prompt type = new file, zero code changes — this
//! module only knows how to load, parse, render, and reset to the built-in default.
//! Edits take effect on the next render because nothing is cached.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::{MaestroError, Result};

/// Built-in defaults, copied to the prompts dir when missing and used by "reset".
/// Adding a template = adding a file here plus one line in this table.
const DEFAULT_TEMPLATES: &[(&str, &str)] = &[
    (
        "line-question",
        include_str!("../../../../prompts-defaults/line-question.md"),
    ),
    (
        "commit-message",
        include_str!("../../../../prompts-defaults/commit-message.md"),
    ),
    (
        "pr-description",
        include_str!("../../../../prompts-defaults/pr-description.md"),
    ),
    (
        "task-notes",
        include_str!("../../../../prompts-defaults/task-notes.md"),
    ),
    (
        "review-fix",
        include_str!("../../../../prompts-defaults/review-fix.md"),
    ),
];

/// One parsed template: informational frontmatter + a `{{var}}` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub variables: Vec<String>,
    pub body: String,
}

/// Loads and renders markdown templates from a directory (`~/.maestro/prompts/` in
/// production; injectable so tests never touch the real filesystem location).
pub struct PromptManager {
    dir: PathBuf,
}

/// A template as the editor sees it: raw markdown plus whether it still matches the
/// built-in default (so the UI can enable "reset" only when it would change something).
#[derive(Clone, Debug, Serialize)]
pub struct PromptFile {
    pub name: String,
    pub description: Option<String>,
    pub variables: Vec<String>,
    /// Full file contents, frontmatter included — what the editor shows.
    pub content: String,
    /// True when a built-in default exists for this name.
    pub has_default: bool,
    /// True when the file differs from that default (i.e. the user edited it).
    pub modified: bool,
}

impl PromptManager {
    /// Ensure `dir` exists and seed it with the built-in default templates — only the
    /// files that are missing; an edited template is never overwritten.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let manager = Self { dir };
        for (name, contents) in DEFAULT_TEMPLATES {
            manager.install_default(name, contents)?;
        }
        Ok(manager)
    }

    /// Built-in default for `name`, if there is one.
    pub fn default_for(name: &str) -> Option<&'static str> {
        DEFAULT_TEMPLATES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, contents)| *contents)
    }

    /// Every template in the prompts dir, sorted by name. Files that fail to read are
    /// skipped with a warning rather than failing the whole listing.
    pub fn list(&self) -> Result<Vec<PromptFile>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match fs::read_to_string(&path) {
                Ok(content) => files.push(self.to_prompt_file(name, content)),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "unreadable prompt template")
                }
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(files)
    }

    /// Raw contents of one template, for the editor.
    pub fn read(&self, name: &str) -> Result<PromptFile> {
        let content = self.read_raw(name)?;
        Ok(self.to_prompt_file(name, content))
    }

    /// Overwrite `name` with `content`. The next render picks it up — no restart, no
    /// cache to invalidate.
    pub fn save(&self, name: &str, content: &str) -> Result<PromptFile> {
        let path = self.path_of(name)?;
        fs::write(&path, content)?;
        tracing::info!(name, path = %path.display(), "prompt template saved");
        Ok(self.to_prompt_file(name, content.to_string()))
    }

    /// Restore the built-in default. Errors for templates that have none (user-created).
    pub fn reset(&self, name: &str) -> Result<PromptFile> {
        let default = Self::default_for(name).ok_or_else(|| MaestroError::Config {
            message: format!("no built-in default for prompt template: {name}"),
        })?;
        self.save(name, default)
    }

    /// Remove a custom template's file. Refuses templates with a built-in
    /// default — "Reset to default" is the right operation for those;
    /// deleting the file would just have it silently reappear as a fresh
    /// default copy the next time `list()`/`new()` reinstalls it.
    pub fn delete(&self, name: &str) -> Result<()> {
        if Self::default_for(name).is_some() {
            return Err(MaestroError::Config {
                message: format!(
                    "\"{name}\" has a built-in default — reset it instead of deleting"
                ),
            });
        }
        let path = self.path_of(name)?;
        fs::remove_file(&path)?;
        tracing::info!(name, path = %path.display(), "prompt template deleted");
        Ok(())
    }

    fn to_prompt_file(&self, name: &str, content: String) -> PromptFile {
        let parsed = parse_template(name, &content);
        let default = Self::default_for(name);
        PromptFile {
            name: name.to_string(),
            description: parsed.description,
            variables: parsed.variables,
            has_default: default.is_some(),
            modified: default.is_some_and(|d| normalize(d) != normalize(&content)),
            content,
        }
    }

    /// Reject names that would escape the prompts directory.
    fn path_of(&self, name: &str) -> Result<PathBuf> {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if !valid {
            return Err(MaestroError::InvalidData {
                message: format!("invalid prompt template name: {name}"),
            });
        }
        Ok(self.dir.join(format!("{name}.md")))
    }

    /// Write `contents` to `<name>.md` in the prompts dir, unless it already exists.
    pub fn install_default(&self, name: &str, contents: &str) -> Result<()> {
        let path = self.dir.join(format!("{name}.md"));
        if !path.exists() {
            fs::write(&path, contents)?;
            tracing::info!(name, path = %path.display(), "installed default prompt template");
        }
        Ok(())
    }

    /// Load and parse `<name>.md`.
    pub fn load(&self, name: &str) -> Result<PromptTemplate> {
        let raw = self.read_raw(name)?;
        Ok(parse_template(name, &raw))
    }

    fn read_raw(&self, name: &str) -> Result<String> {
        let path = self.path_of(name)?;
        fs::read_to_string(&path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                MaestroError::Config {
                    message: format!("prompt template not found: {name} ({})", path.display()),
                }
            } else {
                // Permission denied, invalid UTF-8, ... — report what actually happened.
                MaestroError::Io(err)
            }
        })
    }

    /// Load `name` and render its body with `vars`. Placeholders with no matching key
    /// are left verbatim, so a misconfigured template is visible, not silently dropped.
    pub fn render(&self, name: &str, vars: &HashMap<String, String>) -> Result<String> {
        let template = self.load(name)?;
        Ok(render_body(&template.body, vars))
    }
}

/// Compare template contents ignoring line-ending and trailing-whitespace noise, so a
/// file checked out with CRLF is not reported as edited.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

/// Split `---\n ... \n---` frontmatter from the body. `description:` is taken verbatim;
/// `variables:` accepts a `[a, b, c]` list. Malformed or absent frontmatter just means
/// an empty header — the raw text becomes the body.
fn parse_template(name: &str, raw: &str) -> PromptTemplate {
    let normalized = raw.replace("\r\n", "\n");
    let mut lines = normalized.split('\n').peekable();

    let mut description = None;
    let mut variables = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();

    // A template that opens `---` but never closes it is malformed; the whole file
    // becomes the body rather than vanishing.
    let has_closing_delimiter = normalized.split('\n').skip(1).any(|line| line == "---");

    if lines.peek() == Some(&"---") && has_closing_delimiter {
        lines.next();
        let mut in_frontmatter = true;
        for line in lines {
            if in_frontmatter {
                if line == "---" {
                    in_frontmatter = false;
                    continue;
                }
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim();
                    match key.trim() {
                        "description" => description = Some(value.to_string()),
                        "variables" => {
                            variables = value
                                .trim_start_matches('[')
                                .trim_end_matches(']')
                                .split(',')
                                .map(|v| v.trim().to_string())
                                .filter(|v| !v.is_empty())
                                .collect();
                        }
                        _ => {}
                    }
                }
                continue;
            }
            body_lines.push(line);
        }
    } else {
        body_lines = normalized.split('\n').collect();
    }

    // Drop a single leading blank line right after the closing `---`.
    if body_lines.first() == Some(&"") {
        body_lines.remove(0);
    }

    PromptTemplate {
        name: name.to_string(),
        description,
        variables,
        body: body_lines.join("\n"),
    }
}

/// Substitute every `{{key}}` found in `vars`; unknown placeholders pass through as-is.
fn render_body(body: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        match after_open.find("}}") {
            Some(end) => {
                let key = after_open[..end].trim();
                match vars.get(key) {
                    Some(value) => out.push_str(value),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after_open[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_every_default_and_lists_them() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");

        let listed = manager.list().expect("list");
        let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
        for expected in [
            "commit-message",
            "line-question",
            "pr-description",
            "task-notes",
        ] {
            assert!(names.contains(&expected), "missing default: {expected}");
        }
        assert!(listed.iter().all(|p| p.has_default && !p.modified));
        assert!(
            listed.iter().all(|p| p.description.is_some()),
            "defaults carry a description"
        );
    }

    #[test]
    fn save_marks_modified_and_reset_restores() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");

        let edited = manager
            .save(
                "commit-message",
                "---
name: commit-message
---
Just {{branch}}
",
            )
            .expect("save");
        assert!(
            edited.modified,
            "an edited template is reported as modified"
        );

        // The very next render uses the edit — no restart, nothing cached.
        let mut vars = HashMap::new();
        vars.insert("branch".to_string(), "impl/T-8".to_string());
        assert_eq!(
            manager.render("commit-message", &vars).expect("render"),
            "Just impl/T-8
"
        );

        let reset = manager.reset("commit-message").expect("reset");
        assert!(!reset.modified, "reset returns to the built-in default");
        assert!(reset.content.contains("imperative summary"));
    }

    #[test]
    fn user_templates_have_no_default_to_reset_to() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");
        let custom = manager.save("my-own", "hello {{x}}").expect("save");
        assert!(!custom.has_default);
        assert!(!custom.modified);
        assert!(manager.reset("my-own").is_err(), "nothing to reset to");
    }

    #[test]
    fn delete_removes_a_custom_template_and_it_stops_being_listed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");
        manager.save("my-own", "hello {{x}}").expect("save");
        assert!(manager.list().unwrap().iter().any(|p| p.name == "my-own"));

        manager.delete("my-own").expect("delete");
        assert!(!manager.list().unwrap().iter().any(|p| p.name == "my-own"));
        assert!(manager.read("my-own").is_err(), "the file is really gone");
    }

    #[test]
    fn delete_refuses_a_template_with_a_built_in_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");
        let err = manager.delete("commit-message").unwrap_err();
        assert!(err.to_string().contains("built-in default"), "{err}");
        // Refused, not just erroring cosmetically — the file must still be there.
        assert!(manager.read("commit-message").is_ok());
    }

    #[test]
    fn template_names_cannot_escape_the_prompts_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");
        for bad in ["../evil", "sub/dir", "with space", "", "dots.."] {
            assert!(
                manager.save(bad, "x").is_err(),
                "must reject the name {bad:?}"
            );
        }
    }

    #[test]
    fn crlf_checkout_is_not_reported_as_edited() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = PromptManager::new(tmp.path()).expect("new");
        let default = PromptManager::default_for("task-notes").expect("default exists");
        let crlf = default.replace('\n', "\r\n");
        let saved = manager.save("task-notes", &crlf).expect("save");
        assert!(!saved.modified);
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: line-question\ndescription: Ask about lines\nvariables: [file, question]\n---\nHello {{file}}: {{question}}\n";
        let template = parse_template("line-question", raw);
        assert_eq!(template.description.as_deref(), Some("Ask about lines"));
        assert_eq!(
            template.variables,
            vec!["file".to_string(), "question".to_string()]
        );
        assert_eq!(template.body, "Hello {{file}}: {{question}}\n");
    }

    #[test]
    fn body_without_frontmatter_is_used_verbatim() {
        let template = parse_template("plain", "just a body {{x}}");
        assert!(template.description.is_none());
        assert_eq!(template.body, "just a body {{x}}");
    }

    #[test]
    fn renders_known_vars_and_passes_through_unknown() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        let out = render_body("hello {{name}}, unknown {{missing}}!", &vars);
        assert_eq!(out, "hello world, unknown {{missing}}!");
    }

    #[test]
    fn install_default_copies_once_and_never_overwrites_edits() {
        let dir = tempfile::tempdir().unwrap();
        PromptManager::new(dir.path()).unwrap();
        let path = dir.path().join("line-question.md");
        assert!(path.exists(), "default template installed on first run");

        fs::write(&path, "edited by the user").unwrap();
        // Re-creating the manager (e.g. app restart) must not clobber the edit.
        PromptManager::new(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "edited by the user");
    }

    #[test]
    fn render_uses_the_installed_default_template() {
        let dir = tempfile::tempdir().unwrap();
        let manager = PromptManager::new(dir.path()).unwrap();
        let mut vars = HashMap::new();
        vars.insert("question".to_string(), "why?".to_string());
        vars.insert("branch".to_string(), "impl/T-6-x".to_string());
        vars.insert("file".to_string(), "src/lib.rs".to_string());
        vars.insert("line_start".to_string(), "1".to_string());
        vars.insert("line_end".to_string(), "2".to_string());
        vars.insert("hunk".to_string(), "1: fn x() {}".to_string());
        vars.insert("blame".to_string(), "abc Mock commit".to_string());
        let rendered = manager.render("line-question", &vars).unwrap();
        assert!(rendered.contains("why?"));
        assert!(!rendered.contains("{{question}}"));
    }
}
