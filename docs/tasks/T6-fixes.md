# T6 — fix round (review findings)

Continue in the same worktree/branch (`impl/T-6-string-questions`). The implementation
passed every automated check but was never run in the app; review found it crashes on
mount and mis-attributes answers. Fix all items below, add tests for each, commit.
Never push.

Read `docs/tasks/T6-line-questions.md` again for the original spec and rules.

## 1. CRITICAL — DiffViewer crashes on mount (infinite re-render)

`src/state/questions.ts:62` — `selectQuestions` returns `s.byFile[key] ?? []`. zustand 5
uses a plain `useSyncExternalStore` with no snapshot memoization, so the fresh `[]` for
an absent key makes React see a changed snapshot on every check → "Maximum update depth
exceeded". This happens for every file before any question exists, i.e. the diff viewer
is unusable.

Fix: return a module-level frozen `EMPTY` constant for the missing case (or select the
record and derive with `useShallow`). Verify by reasoning about identity stability, and
add a unit-style test if you can do it without a DOM (otherwise assert selector identity
stability in a plain TS test invoked from `npm run typecheck`-safe code — a simple
`selectQuestions(state) === selectQuestions(state)` assertion in a small vitest-less
script is not required; at minimum keep the constant obvious and commented).

## 2. HIGH — Completion fires on the wrong turn (default `active_session` mode)

Today the core completes the question on **any** `awaiting_input`, and the store marks it
done on any non-streaming status. In the normal case the target session is mid-turn on
its own task: the follow-up is queued, the original turn ends → `awaiting_input` → the
question is marked done, its "answer" is the tail of the unrelated stream, and the real
answer (next turn) is dropped.

Fix — make the core the source of truth for "which question is this session answering":

- Track per pending question an `armed: bool`. Arm it on the first
  `session.status_changed → streaming` for that session **after** the ask; only an
  `awaiting_input`/terminal transition while armed completes the question.
- Publish two new bus events (append to `core/bus/mod.rs`, keep names dotted):
  - `question.answering { question_id, session_id }` when it arms,
  - `question.answered { question_id, session_id, ok }` when it completes (in addition
    to the existing `attention.required`).
- Frontend: route `session.stream_delta` to a question **only** between `answering` and
  `answered` for that session, keyed by `question_id`. This also fixes item 3.
- Interrupt/failed/cancelled paths must complete the question (with `ok: false`) so the
  UI never hangs on "waiting".

## 3. MEDIUM — One delta is appended to questions in every file

`src/state/questions.ts:67` — `updateBySession` walks all `byFile` keys and updates the
first non-done match in each, so asking about file A then file B (same session, the
default) makes both answers identical, and a second question on one file starves until
the first is done.

Fix: index questions by `question_id` (a flat `byId` record plus per-file id lists), and
apply deltas only to the id the core says is currently answering (item 2).

## 4. MEDIUM — Hunk/blame built from the wrong diff scope

`core/questions/mod.rs:96` hardcodes `DiffScope::Worktree`, but the user can select lines
in the "Committed" view, where the editor shows the branch-head content. With uncommitted
edits the line numbers point at different text, and `render_hunk` silently emits blank
lines past EOF.

Fix: add a `scope: DiffScope` parameter to `ask` and to the `ask_line_question` IPC
command; the DiffViewer passes its current scope. Use it for `file_diff`. Blame is
worktree-only — when scope is `branch`, either skip blame (leave `{{blame}}` empty) or
label it clearly; do not silently mix. Also make `render_hunk` return an error (or a
clearly marked truncated hunk) when the requested range exceeds the file length.

## 5. LOW — Stale selection survives editor recreation

`DiffViewer.tsx:221` resets the selection only when `selectedPath` changes, but the
editors are recreated whenever content changes (including automatic reloads on
`diff.updated`), leaving the "Ask about lines 3–5" button and open form bound to shifted
content.

Fix: clear selection + close the ask form whenever the loaded file content changes.

## 6. LOW — Pending question leaks on bus lag

`core/questions/mod.rs:168` only logs on `RecvError::Lagged`. If the skipped events
included the tracked session's transition, the entry never completes.

Fix: on lag, sweep pending entries whose session is terminal in the store (mirror how
`DiffManager::run_invalidation_loop` resyncs), completing them with `ok: false`.

## 7. LOW — Unterminated frontmatter swallows the template

`core/prompts/mod.rs:81` — a file starting with `---` and no closing `---` produces an
empty body, contradicting the function's own doc.

Fix: if no closing delimiter is found, treat the whole file as the body. Add a test.

## 8. MINOR

- Add `tracing` to `ask`/completion with fields (branch, path, lines, session_id,
  target); `warn!` when blame fails instead of a silent `unwrap_or_default()`.
- `core/prompts/mod.rs:56` maps every read error to "prompt template not found" — keep
  the underlying io error (use the typed `MaestroError::Io`).
- Register the pending entry **before** sending/spawning so no status/delta event in the
  gap is missed.
- `core/mod.rs`: keep the module list alphabetical **and** append-only-safe — a parallel
  branch adds `gate` next to your entries.

## Checks

```
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm run lint && npm run typecheck && npx prettier --check src && npx vite build
```

Do not touch `sidecar/`, `core/gate/*`, `src/views/GateDialog.tsx`, `src/state/gates.ts`
(a parallel branch owns those).
