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
    (
        "review-reply-style",
        include_str!("../../../../prompts-defaults/review-reply-style.md"),
    ),
    (
        "review-workflow-gate",
        include_str!("../../../../prompts-defaults/review-workflow-gate.md"),
    ),
    (
        "red-team",
        include_str!("../../../../prompts-defaults/red-team.md"),
    ),
    (
        "review-guide",
        include_str!("../../../../prompts-defaults/review-guide.md"),
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
    /// True when the user edited this template AND the built-in default has
    /// changed since it was installed — resetting would pick up the newer
    /// default, discarding the edit. Unedited templates never show this: they
    /// are auto-updated at startup instead.
    pub update_available: bool,
}

impl PromptManager {
    /// Ensure `dir` exists and seed it with the built-in default templates.
    /// A template the user never edited is kept in sync with the shipped
    /// default (tracked via a hash of the default it was installed from, in
    /// `.installed-defaults.json`); an edited template is never overwritten.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let manager = Self { dir };
        manager.sync_defaults()?;
        Ok(manager)
    }

    /// Install missing defaults and update stale-but-unedited ones. The hash
    /// file records which default each template was installed from; a file
    /// that still matches its recorded default was never touched by the user,
    /// so a newer shipped default may replace it. Anything else is the user's.
    fn sync_defaults(&self) -> Result<()> {
        let mut meta = self.load_meta();
        let mut meta_changed = false;
        for (name, default) in DEFAULT_TEMPLATES {
            let path = self.dir.join(format!("{name}.md"));
            let default_hash = fnv1a(&normalize(default));
            let existing = match fs::read_to_string(&path) {
                Ok(raw) => Some(raw),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    tracing::warn!(name, error = %err, "unreadable prompt template; leaving it alone");
                    continue;
                }
            };
            match existing {
                None => {
                    fs::write(&path, default)?;
                    meta.insert(name.to_string(), default_hash);
                    meta_changed = true;
                    tracing::info!(name, "installed default prompt template");
                }
                Some(raw) => {
                    let file_hash = fnv1a(&normalize(&raw));
                    match meta.get(*name) {
                        // Never edited (still the default it was installed
                        // from) and the shipped default moved → update.
                        Some(installed)
                            if *installed == file_hash && *installed != default_hash =>
                        {
                            fs::write(&path, default)?;
                            meta.insert(name.to_string(), default_hash);
                            meta_changed = true;
                            tracing::info!(name, "default prompt template updated");
                        }
                        Some(_) => {}
                        // Pre-hash installs: adopt the file as "unedited" only
                        // when it already equals the current default; anything
                        // else could be a user edit and stays untouched.
                        None if file_hash == default_hash => {
                            meta.insert(name.to_string(), default_hash);
                            meta_changed = true;
                        }
                        None => {}
                    }
                }
            }
        }
        if meta_changed {
            self.save_meta(&meta);
        }
        Ok(())
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join(".installed-defaults.json")
    }

    fn load_meta(&self) -> HashMap<String, String> {
        fs::read_to_string(self.meta_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Best-effort: losing the hash file only means updates stop flowing until
    /// the next reset — never worth failing template loading over.
    fn save_meta(&self, meta: &HashMap<String, String>) {
        if let Ok(json) = serde_json::to_string_pretty(meta) {
            if let Err(err) = fs::write(self.meta_path(), json) {
                tracing::warn!(error = %err, "could not write .installed-defaults.json");
            }
        }
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
        let file = self.save(name, default)?;
        // The file is the current default again — record that, so future
        // shipped updates keep flowing to it automatically.
        let mut meta = self.load_meta();
        meta.insert(name.to_string(), fnv1a(&normalize(default)));
        self.save_meta(&meta);
        Ok(file)
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
        let modified = default.is_some_and(|d| normalize(d) != normalize(&content));
        // Edited template + a default that moved since it was installed:
        // resetting would pick up the newer default. Unedited ones are
        // auto-updated at startup, so this can only be true for edits.
        let update_available = modified
            && default.is_some_and(|d| {
                self.load_meta()
                    .get(name)
                    .is_some_and(|installed| *installed != fnv1a(&normalize(d)))
            });
        PromptFile {
            name: name.to_string(),
            description: parsed.description,
            variables: parsed.variables,
            has_default: default.is_some(),
            modified,
            update_available,
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

    /// Render arbitrary template text — an editor draft, saved or not — with a
    /// labeled sample value for each declared variable. Never touches disk:
    /// the preview must show the draft, not the last saved file.
    pub fn preview(raw: &str) -> String {
        let template = parse_template("preview", raw);
        let vars: HashMap<String, String> = template
            .variables
            .iter()
            .map(|v| (v.clone(), format!("[sample {v}]")))
            .collect();
        render_body(&template.body, &vars)
    }
}

/// Compare template contents ignoring line-ending and trailing-whitespace noise, so a
/// file checked out with CRLF is not reported as edited.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

/// FNV-1a over the normalized text, as hex. Deliberately hand-rolled: the hash
/// is persisted across app versions, so it must never change algorithm the way
/// `DefaultHasher` is allowed to. (A mismatch would only mean "treated as
/// user-edited" — safe — but stable is better than safe-by-accident.)
fn fnv1a(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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
        assert!(reset.content.contains("imperative mood"));
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
    fn preview_substitutes_declared_variables_with_labeled_samples() {
        let raw = "---\nname: t\nvariables: [branch, files]\n---\nOn {{branch}}:\n{{files}}\nUndeclared {{other}} stays.\n";
        let rendered = PromptManager::preview(raw);
        assert!(rendered.contains("On [sample branch]:"), "{rendered}");
        assert!(rendered.contains("[sample files]"), "{rendered}");
        // Undeclared placeholders keep their braces so the author notices them.
        assert!(rendered.contains("{{other}}"), "{rendered}");
        assert!(
            !rendered.contains("name: t"),
            "frontmatter must be stripped"
        );
    }

    #[test]
    fn renders_known_vars_and_passes_through_unknown() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        let out = render_body("hello {{name}}, unknown {{missing}}!", &vars);
        assert_eq!(out, "hello world, unknown {{missing}}!");
    }

    #[test]
    fn defaults_install_once_and_edits_are_never_clobbered() {
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
    fn a_stale_unedited_default_is_updated_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        PromptManager::new(dir.path()).unwrap();

        // Simulate an older install: the file holds a previous default, and
        // the hash file records exactly that content as what was installed.
        let path = dir.path().join("line-question.md");
        let old_default = "the old shipped default";
        fs::write(&path, old_default).unwrap();
        let meta_path = dir.path().join(".installed-defaults.json");
        let mut meta: HashMap<String, String> =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.insert("line-question".into(), fnv1a(&normalize(old_default)));
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // A restart with a newer shipped default replaces the untouched file.
        PromptManager::new(dir.path()).unwrap();
        let now = fs::read_to_string(&path).unwrap();
        assert_eq!(
            normalize(&now),
            normalize(PromptManager::default_for("line-question").unwrap()),
            "the stale-but-unedited template caught up with the shipped default"
        );
    }

    #[test]
    fn an_edited_template_with_a_moved_default_reports_update_available() {
        let dir = tempfile::tempdir().unwrap();
        let manager = PromptManager::new(dir.path()).unwrap();

        // The user edits; the recorded install hash then claims an older
        // default than the shipped one (simulating an app upgrade after edit).
        manager
            .save("line-question", "my custom question prompt")
            .unwrap();
        let meta_path = dir.path().join(".installed-defaults.json");
        let mut meta: HashMap<String, String> =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.insert("line-question".into(), fnv1a("an older default"));
        fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let file = manager.read("line-question").unwrap();
        assert!(file.modified);
        assert!(
            file.update_available,
            "the editor can now say: the built-in default changed since your edit"
        );

        // Reset picks up the current default and re-records it, so the flag clears.
        let reset = manager.reset("line-question").unwrap();
        assert!(!reset.modified);
        assert!(!reset.update_available);
    }

    #[test]
    fn a_pre_hash_install_matching_the_default_is_adopted_for_updates() {
        let dir = tempfile::tempdir().unwrap();
        PromptManager::new(dir.path()).unwrap();
        // Wipe the hash file: this is what an install from before the feature looks like.
        fs::remove_file(dir.path().join(".installed-defaults.json")).unwrap();

        PromptManager::new(dir.path()).unwrap();
        let meta: HashMap<String, String> = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".installed-defaults.json")).unwrap(),
        )
        .unwrap();
        assert!(
            meta.contains_key("line-question"),
            "files still equal to the default get re-adopted into the update flow"
        );
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
