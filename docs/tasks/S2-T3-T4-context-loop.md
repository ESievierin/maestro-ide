# S2-T3 / S2-T4 — closing the context loop

S2-T1 (`TASK_NOTES.md`) and S2-T2 (`ask_original_agent`) built the two channels. These two
tasks make them fill up and get used. They had no separate spec — the shape follows from
T1's plan (`write()` "used by S2-T3 later for the Q&A append") and Stage 2's goal, so this
document records what was built and why.

Implemented 2026-08-05, together with T1/T2, in the main worktree rather than by parallel
agents: the user was asleep and asked for the whole improvement to be finished, so
dogfooding the parallel path would have needed them at the keyboard.

## S2-T3 — line-question answers are archived, not just chatted

A question about a specific line is exactly the context the next agent wants, and it used to
live only in a chat transcript nobody reads again.

- `core/questions` now keeps the question text and **collects the answer** as it streams
  (only while the question is armed — before that the session's output belongs to whatever
  it was already doing, which is the same rule that fixed the T6 attribution bug).
- On a successful completion the pair is appended to the branch's notes under `## Q&A` as
  `### <path>:<start>-<end>` + `**Q:**` + the answer.
- The append is **section-surgical**: `insert_into_qa` adds the section when missing, joins
  an existing one, and lands the entry _inside_ `## Q&A` even when later sections follow.
  Everything else in the file is left byte-for-byte alone — the agent and the user own those
  sections.
- Archiving is best-effort: no worktree, an unreadable file, or a missing notes manager logs
  a warning and leaves the question successful. The answer is already in the chat; a failed
  archive must not make an answered question look failed.
- The collection lives in the core, not the UI: the frontend has the answer text too, but
  "no business logic in the UI" means the archive is written by whoever owns the lifecycle.

## S2-T4 — review sessions start with the record in hand

A review-fix session did not know anything the implementing agent had learned.

- Starting a session with type `review_fix` now renders the new `review-fix` template around
  what the user typed: the review comments, the branch's `TASK_NOTES.md`, the task id and
  base branch, plus an instruction to question a comment that contradicts a recorded
  decision rather than silently obeying it.
- The same session type gets `tools_profile: "review"` (S2-T2), so `ask_original_agent` is
  available when the notes fall short — notes first, escalation second, both explained in
  the prompt.
- Fallbacks: no template → the comments are sent as-is; no notes → the prompt says so
  explicitly instead of rendering an empty block. A session that starts with less context is
  much better than one that does not start.
- Session type became a real choice in the new-session form, with a line under it saying
  what the chosen type will do (notes on close / escalation tool), because none of this is
  discoverable otherwise.

## What is deliberately not here

- **No PR-comment ingestion.** Pulling review threads from GitHub is a separate concern
  (auth, polling, mapping threads to lines) and Stage 3 territory; pasting the comments is
  the honest version until then.
- **No notes editing in the UI.** The panel reads. Notes are written by the agent that did
  the work, by the Q&A archive, or by hand in the file — an editor in Maestro would be a
  fourth writer with no owner.

## Verification

Standard checks (`cargo fmt/clippy/test`, `lint/typecheck/test/build`, sidecar build/lint)
plus:

- Q&A insertion is unit-tested against the four cases that matter: no section yet, a second
  entry joining the first, an entry landing before a following section, and empty notes.
- Review seeding is tested through `SessionManager::spawn` with the mock engine: the
  comments end up inside the template, the notes placeholder appears when there are none,
  and other session types send their prompt untouched.
