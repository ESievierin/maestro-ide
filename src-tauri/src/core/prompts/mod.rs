//! Minimal prompt template engine (T8 will extend this with more defaults and a UI).
//!
//! Prompts are data: markdown files with frontmatter in `~/.maestro/prompts/`, rendered
//! through `{{var}}` substitution. New prompt type = new file, zero code changes — this
//! module only knows how to load, parse, and render.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::error::{MaestroError, Result};

/// The default `line-question` template (T6), copied to the prompts dir on first run.
const DEFAULT_LINE_QUESTION_TEMPLATE: &str =
    include_str!("../../../../prompts-defaults/line-question.md");

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

impl PromptManager {
    /// Ensure `dir` exists and seed it with the built-in default templates — only the
    /// files that are missing; an edited template is never overwritten.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let manager = Self { dir };
        manager.install_default("line-question", DEFAULT_LINE_QUESTION_TEMPLATE)?;
        Ok(manager)
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
        let path = self.dir.join(format!("{name}.md"));
        let raw = fs::read_to_string(&path).map_err(|_| MaestroError::Config {
            message: format!("prompt template not found: {name} ({})", path.display()),
        })?;
        Ok(parse_template(name, &raw))
    }

    /// Load `name` and render its body with `vars`. Placeholders with no matching key
    /// are left verbatim, so a misconfigured template is visible, not silently dropped.
    pub fn render(&self, name: &str, vars: &HashMap<String, String>) -> Result<String> {
        let template = self.load(name)?;
        Ok(render_body(&template.body, vars))
    }
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

    if lines.peek() == Some(&"---") {
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
