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

    /// Add one question-and-answer pair to the notes' `## Q&A` section, creating the file
    /// and the section when they do not exist yet.
    ///
    /// This is the second way notes get written (the first is an implementation session's
    /// last turn): a question the user asked about a specific line, and what the agent
    /// answered, is exactly the context the next agent will want — and it would otherwise
    /// live only in a chat transcript that nobody reads again.
    pub fn append_qa(
        &self,
        branch: &str,
        context: &str,
        question: &str,
        answer: &str,
    ) -> Result<Notes> {
        let existing = self.read(branch)?;
        if let Some(reason) = existing.unavailable {
            return Err(MaestroError::InvalidData { message: reason });
        }
        let entry = format_qa(context, question, answer);
        let updated = insert_into_qa(&existing.raw, &entry);
        self.write(branch, &updated)
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

/// Heading the Q&A pairs live under.
const QA_HEADING: &str = "## Q&A";

/// One Q&A entry: where it was asked about, the question, the answer.
fn format_qa(context: &str, question: &str, answer: &str) -> String {
    let answer = answer.trim();
    let question = question.trim();
    format!("### {context}\n\n**Q:** {question}\n\n{answer}\n")
}

/// Put `entry` at the end of the `## Q&A` section, adding the section when it is missing.
/// Everything else in the file is left exactly as it was — the user (and the agent that
/// wrote the rest) own those sections.
fn insert_into_qa(raw: &str, entry: &str) -> String {
    let trimmed = raw.trim_end();
    match section_bounds(trimmed, QA_HEADING) {
        Some(end) => {
            let (before, after) = trimmed.split_at(end);
            format!(
                "{}\n\n{}{}",
                before.trim_end(),
                entry.trim_end(),
                tail(after)
            )
        }
        None => {
            let separator = if trimmed.is_empty() { "" } else { "\n\n" };
            format!("{trimmed}{separator}{QA_HEADING}\n\n{}\n", entry.trim_end())
        }
    }
}

/// Byte offset where the named section's body ends (the next `## ` heading, or the end).
fn section_bounds(raw: &str, heading: &str) -> Option<usize> {
    let start = raw
        .lines()
        .scan(0usize, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .find(|(_, line)| line.trim_end() == heading)
        .map(|(at, _)| at)?;

    let after_heading = start + heading.len();
    let next = raw[after_heading..]
        .lines()
        .scan(after_heading, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .find(|(at, line)| *at > after_heading && line.starts_with("## "))
        .map(|(at, _)| at);
    Some(next.unwrap_or(raw.len()))
}

/// The remainder of the file after an insertion point, kept verbatim.
fn tail(after: &str) -> String {
    if after.trim().is_empty() {
        "\n".to_string()
    } else {
        format!("\n\n{}\n", after.trim())
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

    /// A real repo, because the whole point of this module is finding the file in a
    /// worktree — a mocked path would test nothing that can break.
    fn repo_with_manager() -> (tempfile::TempDir, Arc<NotesManager>, String) {
        use crate::core::worktree::{GitCli, WorktreeManager};
        use std::process::Command;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir");
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "test@maestro.local"],
            vec!["config", "user.name", "Maestro Test"],
        ] {
            let out = Command::new("git")
                .current_dir(&repo)
                .args(&args)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        }
        std::fs::write(
            repo.join("README.md"),
            "hello
",
        )
        .expect("write");
        for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
            Command::new("git")
                .current_dir(&repo)
                .args(&args)
                .output()
                .expect("git");
        }

        let bus = EventBus::new();
        let store = Arc::new(crate::core::store::SqliteStore::open_in_memory().unwrap());
        let worktrees = Arc::new(WorktreeManager::new(
            Arc::new(GitCli),
            store.clone(),
            bus.clone(),
        ));
        worktrees.set_repo(&repo).expect("select repo");
        let branch = worktrees
            .list()
            .expect("list")
            .into_iter()
            .find_map(|w| w.branch)
            .expect("the primary worktree's branch");
        (tmp, Arc::new(NotesManager::new(worktrees, bus)), branch)
    }

    #[test]
    fn notes_round_trip_through_a_real_worktree() {
        let (_tmp, notes, branch) = repo_with_manager();

        // Nothing written yet: a state, not an error, and the path is known.
        let empty = notes.read(&branch).expect("read");
        assert!(!empty.exists);
        assert!(empty.unavailable.is_none());
        assert!(empty.path.unwrap().ends_with(NOTES_FILE));
        assert_eq!(notes.current_text(&branch), None);

        let written = notes.write(&branch, WELL_FORMED).expect("write");
        assert!(written.exists);
        assert_eq!(written.sections.len(), 4);
        assert!(written.updated_at.is_some());
        assert!(notes.current_text(&branch).is_some());

        // The Q&A archive lands in the file, next to what was already there.
        let updated = notes
            .append_qa(
                &branch,
                "src.rs:1-2",
                "why three retries?",
                "The gateway retries twice.",
            )
            .expect("append");
        assert!(updated.raw.contains("## Q&A"));
        assert!(updated.raw.contains("### src.rs:1-2"));
        assert!(updated.raw.contains("The gateway retries twice."));
        assert!(updated
            .raw
            .contains("- Chose SQLite over a file, because queries."));
        // And it really is on disk, not just in the returned struct.
        let on_disk = std::fs::read_to_string(updated.path.unwrap()).expect("read file");
        assert_eq!(on_disk, updated.raw);
    }

    #[test]
    fn an_unknown_branch_is_unavailable_rather_than_an_error() {
        let (_tmp, notes, _branch) = repo_with_manager();
        let notes_for_ghost = notes.read("impl/does-not-exist").expect("read");
        assert!(!notes_for_ghost.exists);
        assert!(notes_for_ghost
            .unavailable
            .expect("a reason")
            .contains("no worktree"));
        // Writing there is an error, because the caller asked for something impossible.
        assert!(notes.write("impl/does-not-exist", "x").is_err());
        assert!(notes
            .append_qa("impl/does-not-exist", "a", "b", "c")
            .is_err());
    }

    #[test]
    fn a_qa_entry_creates_the_section_when_it_is_missing() {
        let updated = insert_into_qa(
            WELL_FORMED,
            &format_qa("src/a.rs:10-12", "why?", "because."),
        );
        assert!(updated.contains("## Q&A"));
        assert!(updated.contains("### src/a.rs:10-12"));
        assert!(updated.contains("**Q:** why?"));
        assert!(updated.contains("because."));
        // The sections that were already there survive untouched.
        assert!(updated.contains("## Decisions"));
        assert!(updated.contains("- Chose SQLite over a file, because queries."));
    }

    #[test]
    fn a_second_qa_entry_joins_the_existing_section() {
        let first = insert_into_qa(WELL_FORMED, &format_qa("src/a.rs:1-2", "q one", "a one"));
        let second = insert_into_qa(&first, &format_qa("src/b.rs:3-4", "q two", "a two"));
        assert_eq!(
            second.matches("## Q&A").count(),
            1,
            "one section, two entries"
        );
        assert!(second.find("q one").unwrap() < second.find("q two").unwrap());
        // And the Q&A section stays the last one, not spliced into another.
        let sections = parse_sections(&second);
        assert_eq!(sections.last().unwrap().title, "Q&A");
    }

    #[test]
    fn a_qa_entry_lands_before_a_following_section() {
        let raw = "## Q&A

### old.rs:1-1

**Q:** old?

old answer

## Open questions

- none yet
";
        let updated = insert_into_qa(raw, &format_qa("new.rs:2-2", "new?", "new answer"));
        let qa_at = updated.find("new answer").expect("the new entry");
        let next_at = updated
            .find("## Open questions")
            .expect("the later section");
        assert!(
            qa_at < next_at,
            "the entry belongs inside Q&A:
{updated}"
        );
        assert!(updated.contains("old answer"), "the old entry survives");
    }

    #[test]
    fn notes_that_do_not_exist_yet_start_with_the_qa_section() {
        let updated = insert_into_qa("", &format_qa("src/a.rs:1-1", "why?", "because."));
        assert!(updated.starts_with("## Q&A"));
    }

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
