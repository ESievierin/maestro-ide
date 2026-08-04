//! `TASK_NOTES.md` — the written record a task leaves behind (S2-T1).
//!
//! The point of this file is cross-agent context: an agent answering PR review comments
//! must be able to read why the implementing agent did what it did. The notes live in the
//! worktree as an ordinary committed file, so they travel with the branch, show up in the
//! diff like any other change, and survive Maestro entirely.
//!
//! **Read strategy: refresh on read, no file watcher.** Notes change rarely — once per
//! finalized session, plus the occasional manual edit — and a watcher would be a
//! background subsystem with its own failure modes for no real gain. The consequence is
//! explicit: an external change (a manual edit, `git checkout`, a merge) becomes visible on
//! the next refresh, not instantly. Callers refresh when the panel opens, on the Refresh
//! button, and when a session on that branch finishes.
//!
//! Nothing about notes is persisted in the store: everything here is derived from the
//! filesystem, so a missing worktree is a *state* ([`Notes::exists`] false with a reason),
//! never an error.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::bus::{Event, EventBus};
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// The notes file's name in the worktree root. Committed like any other file.
pub const NOTES_FILE: &str = "TASK_NOTES.md";

/// One `##` section of the notes. Unknown sections are kept: the file is user-editable and
/// losing content nobody expected would be worse than rendering it verbatim.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NoteSection {
    pub title: String,
    pub body: String,
}

/// The notes of one branch, as read from its worktree.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Notes {
    pub branch: String,
    /// Absolute path the notes would live at, when a worktree exists.
    pub path: Option<PathBuf>,
    pub exists: bool,
    /// Why the notes are unavailable, when they are (no worktree, unreadable file).
    pub unavailable: Option<String>,
    pub sections: Vec<NoteSection>,
    /// The whole file, so the UI can render markdown rather than re-assemble sections.
    pub raw: String,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Notes {
    fn unavailable(branch: &str, reason: impl Into<String>) -> Self {
        Self {
            branch: branch.to_string(),
            path: None,
            exists: false,
            unavailable: Some(reason.into()),
            sections: Vec::new(),
            raw: String::new(),
            updated_at: None,
        }
    }
}

pub struct NotesManager {
    worktrees: Arc<WorktreeManager>,
    bus: EventBus,
}

impl NotesManager {
    pub fn new(worktrees: Arc<WorktreeManager>, bus: EventBus) -> Self {
        Self { worktrees, bus }
    }

    /// Read the notes of `branch`. Never fails because the notes are missing.
    pub fn read(&self, branch: &str) -> Result<Notes> {
        let path = match self.notes_path(branch)? {
            Some(path) => path,
            None => {
                return Ok(Notes::unavailable(
                    branch,
                    format!("no worktree for {branch} — notes live in the worktree"),
                ))
            }
        };

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Notes {
                    branch: branch.to_string(),
                    path: Some(path),
                    exists: false,
                    unavailable: None,
                    sections: Vec::new(),
                    raw: String::new(),
                    updated_at: None,
                });
            }
            Err(err) => {
                // A file that exists but cannot be read is worth reporting as a state, not
                // an error: the panel says why instead of the app raising a toast.
                return Ok(Notes::unavailable(branch, format!("{NOTES_FILE}: {err}")));
            }
        };

        let updated_at = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .map(DateTime::<Utc>::from);

        Ok(Notes {
            branch: branch.to_string(),
            path: Some(path),
            exists: true,
            unavailable: None,
            sections: parse_sections(&raw),
            raw,
            updated_at,
        })
    }

    /// Replace the notes of `branch` and announce it. Used by the Q&A append (S2-T3) and
    /// by anything else that needs to write the record without shelling out to an agent.
    pub fn write(&self, branch: &str, raw: &str) -> Result<Notes> {
        let path = self
            .notes_path(branch)?
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for {branch} — cannot write {NOTES_FILE}"),
            })?;
        std::fs::write(&path, raw)?;
        tracing::info!(branch, bytes = raw.len(), "task notes written");
        self.bus.publish(Event::NotesUpdated {
            branch: branch.to_string(),
        });
        self.read(branch)
    }

    /// Notes content for prompt rendering: the file, or `None` when there is nothing yet.
    pub fn current_text(&self, branch: &str) -> Option<String> {
        match self.read(branch) {
            Ok(notes) if notes.exists && !notes.raw.trim().is_empty() => Some(notes.raw),
            _ => None,
        }
    }

    fn notes_path(&self, branch: &str) -> Result<Option<PathBuf>> {
        let worktree = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch));
        Ok(worktree.map(|w| w.path.join(NOTES_FILE)))
    }
}

/// Split markdown into its `##` sections. Content before the first heading is kept under an
/// empty title so nothing is dropped, and CRLF input parses the same as LF.
fn parse_sections(raw: &str) -> Vec<NoteSection> {
    let mut sections: Vec<NoteSection> = Vec::new();
    let mut title = String::new();
    let mut body: Vec<&str> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim_end_matches('\r');
        match trimmed.strip_prefix("## ") {
            Some(heading) => {
                push_section(&mut sections, &title, &body);
                title = heading.trim().to_string();
                body.clear();
            }
            None => body.push(trimmed),
        }
    }
    push_section(&mut sections, &title, &body);
    sections
}

fn push_section(sections: &mut Vec<NoteSection>, title: &str, body: &[&str]) {
    let text = body.join("\n").trim().to_string();
    // A preamble with no heading is only worth keeping when it actually says something.
    if title.is_empty() && text.is_empty() {
        return;
    }
    sections.push(NoteSection {
        title: title.to_string(),
        body: text,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const WELL_FORMED: &str = "\
# Task notes

## Decisions

- Chose SQLite over a file, because queries.

## Trade-offs

- No file watcher: notes refresh on read.

## Open questions

- none yet
";

    #[test]
    fn parses_the_three_sections() {
        let sections = parse_sections(WELL_FORMED);
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["", "Decisions", "Trade-offs", "Open questions"]);
        assert_eq!(sections[0].body, "# Task notes");
        assert!(sections[1].body.contains("SQLite"));
        assert_eq!(sections[3].body, "- none yet");
    }

    #[test]
    fn keeps_sections_it_did_not_expect() {
        let raw = format!("{WELL_FORMED}\n## Notes from the user\n\n- keep me\n");
        let sections = parse_sections(&raw);
        let last = sections.last().expect("a section");
        assert_eq!(last.title, "Notes from the user");
        assert_eq!(last.body, "- keep me");
    }

    #[test]
    fn crlf_parses_like_lf() {
        let crlf = WELL_FORMED.replace('\n', "\r\n");
        assert_eq!(parse_sections(&crlf), parse_sections(WELL_FORMED));
    }

    #[test]
    fn empty_input_has_no_sections() {
        assert!(parse_sections("").is_empty());
        assert!(parse_sections("\n\n  \n").is_empty());
    }

    #[test]
    fn heading_without_body_is_kept() {
        let sections = parse_sections("## Decisions\n");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Decisions");
        assert_eq!(sections[0].body, "");
    }
}
