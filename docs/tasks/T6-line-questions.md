# T6 — Line-level questions in diffs

You are implementing task T6 of MaestroIDE inside a dedicated git worktree. Work only in
this worktree. Do not run `npm run tauri dev`. Commit your work at the end (small logical
commits are fine); never push.

## Project context

MaestroIDE is a Tauri 2 desktop app orchestrating parallel Claude Code agents on git
worktrees. Read `README.md` and `maestro-stage1-prompt.md` at the repo root first.

Non-negotiable architecture rules (from the project brief):
- All logic lives in the Rust core (`src-tauri/src/core/*`). The frontend only renders
  state and sends commands.
- Every state change is an event on the central bus (`core/bus`); UI panels subscribe.
  Never call UI logic from core modules.
- Typed errors (`src-tauri/src/error.rs`), no `unwrap()` in core paths, structured
  logging via `tracing` with fields.
- Prompts are data: markdown files with frontmatter in `~/.maestro/prompts/`, rendered
  through one template engine with `{{var}}` substitution. New prompt type = new file.

What already exists and works (do not rebuild):
- `core/diff::DiffManager` — diff snapshots per branch+scope, `blame(branch, path,
  start, end)` (via `git blame --line-porcelain` in the worktree), `file_diff`.
- `core/session::SessionManager` — `spawn(SpawnParams)` (params include `branch`, `cwd`,
  `session_type`, `model`, `effort`, `permission_mode`, `prompt`, `resume_from`),
  `send(session_id, prompt)`, `list_for_branch`. Sessions stream back via bus events
  `session.stream_delta` / `session.status_changed` / `session.tool_use`.
- IPC commands in `src-tauri/src/ipc/mod.rs` (pattern: thin async commands via
  `run_core`), registered in `src-tauri/src/lib.rs`.
- Frontend: `src/views/DiffViewer.tsx` (CodeMirror 6, split `MergeView` + unified
  `unifiedMergeView`, file list, scope toggle), zustand stores in `src/state/*` fed by
  bus events through `onBusEvent` (`src/state/events.ts`).

## Task (from the Stage 1 brief)

Select lines in the diff → build context (file path, hunk text, blame, branch) → render
a prompt from the `line-question` template → send it to the worktree's most recent
active session via a follow-up message, or to a fresh short-lived session (config
option). Answers render as inline blocks between diff lines (simple block insertion —
no floating anchored bubbles). If the user navigated away before the answer arrived →
`attention.required` event.

DoD: select → ask → answer appears inline; works on several worktrees in parallel.

## Implementation plan

### 1. `core/prompts` module (minimal template engine — T8 will extend it)

- `src-tauri/src/core/prompts/mod.rs`: load templates from `~/.maestro/prompts/*.md`.
  Frontmatter between `---` lines with `name:`, `description:`, `variables:` (informational).
  Body uses `{{var}}` placeholders; render with a `HashMap<String, String>`;
  unknown placeholders stay verbatim.
- Embed a default `line-question` template (include_str! from
  `prompts-defaults/line-question.md`, which you create) and copy it to
  `~/.maestro/prompts/` on first run if missing (do this in `PromptManager::new` or
  similar, called from `lib.rs`).
- Default template body should produce a focused question prompt with variables:
  `{{branch}}`, `{{file}}`, `{{line_start}}`, `{{line_end}}`, `{{hunk}}` (the selected
  lines with line numbers), `{{blame}}` (rendered blame lines: sha author summary),
  `{{question}}`.
- Unit tests: frontmatter parsing, rendering with vars, unknown var passthrough,
  default template copy-if-missing (use a temp dir override for the prompts dir — make
  the directory injectable in the constructor).

### 2. Line-question flow in core

- New module `src-tauri/src/core/questions/mod.rs` (or fold into diff — your call, keep
  it small): `LineQuestionManager` with `ask(branch, path, start, end, question) ->
  Result<LineQuestionInfo>`:
  - Build context: file lines from `DiffManager::file_diff` (new side) for the hunk
    text; `DiffManager::blame` for blame lines.
  - Render the `line-question` template.
  - Target resolution, controlled by settings key `line_question_target` with values
    `"active_session"` (default) | `"fresh_session"`:
    - active_session: most recently updated non-terminal session of the branch
      (`SessionManager::list_for_branch`); send the rendered prompt as a follow-up.
      Fall back to a fresh session when none is live.
    - fresh_session: spawn a session with `session_type: research`,
      `permission_mode: "plan"` (read-only), the rendered prompt as initial prompt, and
      cwd = the branch's worktree path.
  - Track pending questions (map session_id → question metadata: branch, path, lines,
    question, asked_at). On that session's `session.status_changed` →
    `awaiting_input`/terminal (subscribe to the bus like
    `DiffManager::run_invalidation_loop` does), publish
    `attention.required { source: "line_question", branch, session_id, message }` and
    clear the entry. (The UI decides whether the user is still looking; core always
    announces completion.)
  - Return `LineQuestionInfo { question_id, session_id, branch, path, line_start,
    line_end, question }` so the UI can bind the inline block to the session stream.
- IPC: `ask_line_question(branch, path, start, end, question)` command; register it in
  `lib.rs`. Append at the end of `ipc/mod.rs`; do not reorder existing code.
- Unit tests with the existing mock patterns (see `core/diff/mod.rs` tests for a full
  `GitProvider` mock and `core/session/manager.rs` tests for a mock `AgentEngine`):
  target selection (active vs fresh vs fallback), attention event on completion.

### 3. Frontend: selection → ask → inline answer

- `src/state/questions.ts`: zustand store: `byFile: Record<branch|path, LineQuestion[]>`
  where each question tracks `{id, sessionId, lineEnd, question, answer, status:
  "waiting"|"streaming"|"done"}`. Subscribe via `onBusEvent`:
  - `session.stream_delta` for a tracked sessionId → append to `answer` (fresh-session
    mode: all deltas; active-session mode: deltas after ask time — keep it simple, just
    collect from ask onward).
  - `session.status_changed` → `awaiting_input`/terminal for a tracked session → mark done.
- DiffViewer:
  - Track the user's line selection in the **new** (right/unified) editor via
    `EditorView.updateListener` — map the main selection to 1-based line numbers.
  - When a selection of ≥1 line exists, show an "Ask about lines N–M" button in the
    diff toolbar (no floating UI). Clicking opens a small inline form (question input +
    Ask/Cancel) above the editor.
  - Submit → `ask_line_question`; insert an inline **CodeMirror block widget**
    (Decoration.widget, `block: true`, `side: 1`) below the last selected line showing
    the question + streaming answer text (plain text is fine; keep the widget DOM
    simple). Manage widgets with a StateField + StateEffect; update the widget content
    as the store streams (dispatch effects or fully reconfigure — simplest correct
    approach wins; re-creating the editor on answer updates is NOT acceptable, it loses
    scroll position).
  - Answers persist per file while the app runs (store keyed by branch+path); reopening
    the file re-inserts the blocks at their line positions.
- Keep changes append-only in `src/types/events.ts` if you need new payload types.

### 4. Checks (all must pass)

```
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm install && npm run lint && npm run typecheck && npx prettier --check src && npx vite build
```

(`npm install` is needed once — worktrees don't inherit node_modules. Do NOT touch
`sidecar/`.)

## Conflict avoidance (T7 runs in a parallel worktree)

- Do not modify: `core/gate/*` (doesn't exist yet — don't create it), `core/agent/*`,
  `core/session/manager.rs` internals (subscribe to the bus instead of hooking the
  manager), `sidecar/*`, `src/views/SessionPanel.tsx`, `.github/*`.
- In shared files (`ipc/mod.rs`, `lib.rs`, `core/mod.rs`, `core/bus/mod.rs`,
  `src/types/events.ts`, `src/state/events.ts`): only append new items at the end of
  the relevant sections; never reformat or reorder existing code.
- `styles.css`: append a clearly-commented section at the end.
