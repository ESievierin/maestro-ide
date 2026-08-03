# S2-T1 — TASK_NOTES.md lifecycle

You are implementing task S2-T1 of MaestroIDE inside a dedicated git worktree. Work only
in this worktree. Commit your work at the end; never push. **Do not run
`npm run tauri dev` in a way that leaves it running** — but you must run the app once in
mock mode to verify (see Checks).

## Project context

MaestroIDE is a Tauri 2 desktop app orchestrating parallel Claude Code agents on git
worktrees. Read `docs/STATUS.md` first (architecture map, conventions, verification
commands), then `maestro-stage1-prompt.md` for the Stage 1 principles — they all still
apply: logic in the Rust core, every state change is a bus event, traits on boundaries,
prompts are data, typed errors, structured logging, no logic in the UI.

Stage 2 goal for context: an agent answering PR review comments must be able to read the
reasoning of the agent that implemented the work. `TASK_NOTES.md` is the primary channel;
a live escalation tool (built in parallel as S2-T2) is the fallback. **This task builds
the notes artifact and its lifecycle.**

What already exists and you must reuse, not rebuild:

- `core/prompts::PromptManager` — templates in `~/.maestro/prompts`, `{{var}}` rendering,
  `list/read/save/reset`. The `task-notes` template already exists (`prompts-defaults/`)
  with `## Decisions` / `## Trade-offs` / `## Open questions` and variables
  `{{branch}} {{task_id}} {{base}} {{notes}}`.
- `core/session::SessionManager` — `spawn`, `send(session_id, prompt)`, `close`,
  `list_for_branch`, state machine `spawning → streaming ⇄ awaiting_input → done |
failed | cancelled`. Sessions stream via bus events.
- `core/worktree::WorktreeManager::list()` → worktree paths per branch.
- `core/diff::DiffManager` — the diff viewer's data; `DiffScope::{Branch, Worktree}`.
- Frontend: zustand stores in `src/state/*` fed by `onBusEvent`, `src/views/DiffViewer.tsx`
  (CodeMirror split/unified diff, file list, scope + view toggles).

## Decisions already made — implement these, do not re-litigate

1. **Write path is core-driven, not an SDK hook.** A Stop-hook would need new sidecar
   protocol and would run outside the chat transcript. Instead the _close_ of an
   `implementation` session gets a finalize step (details below).
2. **Read path is refresh-on-read**, no file watcher: notes change rarely, and a watcher
   is a background subsystem Stage 2 does not need. Re-read on `session.status_changed`
   (`done`) for that branch, on explicit refresh, and when the notes panel opens.
   Document this choice in the module doc comment, including the consequence: an external
   edit (git checkout, manual edit) shows up on the next refresh, not instantly.
3. **No new store columns.** Notes live in the worktree at `TASK_NOTES.md`; everything is
   derived from the filesystem. Absent worktree → notes unavailable (a state, not an
   error).
4. `TASK_NOTES.md` is an ordinary committed file: it must appear in the diff like any
   other change. Do not filter it out anywhere.

## Implementation plan

### 1. `core/notes` module

`src-tauri/src/core/notes/mod.rs` with a `NotesManager` holding `Arc<WorktreeManager>`
and the bus:

- `read(branch) -> Result<Notes>` where
  `Notes { branch, path, exists: bool, sections: Vec<NoteSection>, raw: String, updated_at: Option<DateTime<Utc>> }`
  and `NoteSection { title: String, body: String }`.
  - Resolve the worktree via `WorktreeManager::list()`; no worktree → `Notes { exists:
false, .. }` with an explanatory field, never an `Err`.
  - Parse `##` headings into sections, keeping unknown sections (the file is
    user-editable; do not lose content you did not expect).
- `write(branch, raw) -> Result<Notes>` — used by S2-T3 later for the Q&A append; keep it
  small and public. Publish `notes.updated { branch }` (append the event to
  `core/bus/mod.rs`).
- Unit tests: parse a well-formed file, a file with extra sections, an empty/missing file,
  CRLF input; missing worktree.

### 2. Finalize step on session close

In `core/session/manager.rs` — **only inside `close()` and a new private helper; do not
touch `spawn()`/`SpawnParams`, a parallel task owns those.**

- When `close(session_id)` is called on a session whose `session_type` is
  `Implementation` and which is currently `awaiting_input` (i.e. it can still take a
  turn), first send a finalize prompt rendered from the `task-notes` template
  (`{{branch}}`, `{{task_id}}` from the branch row, `{{base}}`, `{{notes}}` = current
  notes content or "none yet"), mark the session as finalizing, and close it only when
  that turn completes (`awaiting_input` again) or a timeout elapses (config setting
  `notes_finalize_timeout_secs`, default 120).
- Sessions that are streaming, already terminal, or of another type close exactly as they
  do today — no behaviour change.
- If the finalize send fails (sidecar gone, session vanished): log a warning, close
  normally. Notes are best-effort; never block or fail a close because of them.
- The prompt must tell the agent to write/update `TASK_NOTES.md` **in the worktree root**
  with the three sections, and that this is its last turn. Extend the `task-notes`
  default template if its current wording does not say that (it currently reads like a
  notes-rewriting instruction — adjust it, and keep the frontmatter variables in sync).
- Tests with the existing `MockEngine` pattern (see the tests in that file): finalize
  prompt is sent for an implementation session, not for `manual`/`research`; close still
  completes when the finalize turn never finishes (timeout path — inject a short timeout);
  a terminal session closes without a finalize prompt.

### 3. IPC + wiring

- Commands (append at the end of `ipc/mod.rs`, register in `lib.rs`):
  `get_notes(branch) -> Notes`, `refresh_notes(branch) -> Notes`.
- Construct `NotesManager` in `lib.rs`, add to `AppState`, pass to `SessionManager` (it
  needs notes for `{{notes}}` in the finalize prompt) — a constructor parameter is fine,
  keep it additive.

### 4. UI: notes panel

- `src/types/notes.ts`, `src/state/notes.ts` (zustand, fed by `notes.updated`),
  `src/views/NotesPanel.tsx`.
- Reachable from the diff view: add a **Notes** toggle to the existing diff toolbar (next
  to Working tree / Committed and Split / Unified) that swaps the editor area for the
  notes render, or a third main tab next to Chat/Diff — your call, pick the one that reads
  better and say which you chose. Read-only markdown render (reuse `react-markdown` as
  `SessionPanel` does), a Refresh button, and an explicit empty state ("no TASK_NOTES.md
  yet — it is written when an implementation session closes").
- **zustand 5 has no snapshot memoization**: a selector must return a value already in
  state. Returning `?? []` or a `.map(...)` result crashes the app with an infinite
  re-render — this has bitten this project twice. See `src/state/questions.ts` and its
  test for the pattern to copy.

## Checks (all must pass)

```
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm install && npm run lint && npm run typecheck && npx prettier --check src && npm test && npx vite build
```

`npm install` is needed once — worktrees don't inherit `node_modules`. The Rust toolchain
is pinned by `rust-toolchain.toml`; the first cargo command may download it.

**Then run the app and look at it** — green checks have twice hidden a UI that crashes on
mount:

```powershell
cd sidecar; npm install; npm run build; cd ..
$env:MAESTRO_SIDECAR_MOCK="1"; npm run tauri dev
```

Open a worktree, open the diff, open the notes panel, confirm no console errors and that
the empty state renders. Then stop the app.

## Conflict avoidance (S2-T2 runs in a parallel worktree)

- Do not touch: `sidecar/*`, `core/escalation/*` (does not exist — do not create),
  `core/agent/*`, `core/gate/*`, `core/session/mod.rs`, `src/views/SessionPanel.tsx`,
  `src/state/sessions.ts`, `.github/*`, `src-tauri/src/core/store/migrations.rs`.
- `core/session/manager.rs` is shared: you own `close()` + your private helpers **only**.
  Do not reformat or reorder anything else in that file.
- In shared files (`ipc/mod.rs`, `lib.rs`, `core/mod.rs`, `core/bus/mod.rs`,
  `src/types/events.ts`, `src/App.tsx`, `src/styles.css`) only append at the end of the
  relevant section; never reorder existing entries.
