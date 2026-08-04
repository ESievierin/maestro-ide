# MaestroIDE — project status snapshot

Living handoff document: where Stage 1 stands, what the moving parts are, and the
conventions that are easy to lose between sessions. Update it when a task lands.

Last updated: 2026-08-05 (Stage 1 complete; Stage S3 Tier 1 — session parity — landed).

## Stage 1 progress

| Task                       | State                                                                 |
| -------------------------- | --------------------------------------------------------------------- |
| T1 skeleton                | done — bus, IPC bridge, SQLite+migrations, typed errors, tracing, CI  |
| T2 GitProvider + worktrees | done — trait + git CLI impl, worktree manager, WorktreeList UI        |
| T3 sidecar + sessions      | done — NDJSON protocol v1, supervisor, session state machine          |
| T4 chat panel              | done — markdown, tool-use folding, single-writer, commands, resume    |
| T5 diff engine + viewer    | done — snapshot cache, CM6 split/unified, worktree & committed scopes |
| T6 line questions          | done (agent-built, reviewed, fixed) — see "Fix rounds"                |
| T7 commit/PR gate          | done (agent-built, reviewed, fixed) — see "Fix rounds"                |
| T8 prompt templates        | done — 4 defaults, list/save/reset, PromptEditor in the header        |
| T9 attention panel         | done — bus-derived queue, status badges, OS notifications (opt-in)    |
| T10 polish / dogfood       | done — config.toml, error toasts, hotkeys, empty states               |

Branch `main` holds everything; the T6/T7 agent branches are merged. The remote is
`ESievierin/maestro-ide` (private, separate account — see "Git identity" below).

## Stage S3 progress (Claude Code parity in sessions)

Inventory and per-item detail: `docs/tasks/S3-parity.md`.

| Tier                                        | State                                                          |
| ------------------------------------------- | -------------------------------------------------------------- |
| Tier 1 runtime switching + dialogs + models | done — protocol v2, see below                                  |
| Tier 2 transcript gaps                      | done — protocol v3: thinking, results, subagents, tasks, usage |
| Tier 3 completeness                         | done except rewind (blocked — see below)                       |

Tier 1 in one paragraph: model / effort / permission mode can be switched **while a
session runs** (session toolbar selectors, or `/model`, `/effort`, `/permissions` in the
chat with autocompleted arguments — that is where model ids are discoverable); the change
is validated, persisted, and announced as `session.settings_changed`. `AskUserQuestion`
now works: the sidecar declares `supportedDialogKinds`, forwards the dialog to the core,
and the UI renders it (`QuestionDialog`), with a 5-minute auto-cancel so a turn can never
park forever.

Tier 2 in one paragraph: the transcript now shows what the agent actually did — folded
reasoning, tool results matched to their calls, subagent activity nested under its `Task`,
the task checklist, auto-denied calls, and per-session cost plus a context meter. Thinking
needed a knob of its own: the CLI default produced none, and a budget without
`display: "summarized"` yields an empty block (see `docs/tasks/S3-parity.md`).

Tier 3 in one paragraph: plan review became a proper dialog (approving it claims the
branch's writer slot, so a plan cannot be approved into a second writer), MCP servers and
subagent profiles are listed and controllable per session, MCP elicitations are answerable,
and pasted images are sent as attachments. **Rewind is blocked**: `rewindFiles` needs a user
message uuid and this CLI emits no user-message replays to SDK consumers, so there is no
checkpoint id — and a worktree plus `git checkout` already covers undoing edits.

Stage 2 (cross-agent context) is specified but not started: `docs/tasks/S2-T1-task-notes.md`,
`docs/tasks/S2-T2-escalation.md`.

## Architecture map (where things live)

- `src-tauri/src/core/bus` — typed `Event` enum, tokio broadcast. Every state change is
  an event; UI panels and future daemon subscribe. Frontend receives them on the single
  Tauri channel `maestro:event` (`ipc::spawn_event_forwarder`).
- `core/store` — `Store` trait + SQLite impl, migrations append-only in
  `store/migrations.rs` (currently 5, the last adding `sessions.thinking`). Branch name is
  the primary key.
- `core/worktree` — `GitProvider` trait (git CLI impl `GitCli`), `WorktreeManager`
  (repo selection persisted in settings, branch naming `{type}/{task-id}-{slug}`,
  worktrees at `<repo>.worktrees/<branch>`).
- `core/agent` — `AgentEngine` trait + `SidecarEngine` supervisor (launch, reader
  threads, request/ack correlation, crash signal, lazy restart).
- `core/session` — `Session` entity, validated state machine, `SessionManager`
  (spawn/send/interrupt/close/delete, single-writer rule, resume via stored
  `sdk_session_id`, stale-session sweep, gate hook in `PermissionRequest`).
- `core/diff` — `DiffManager`: per (branch, scope) snapshot cache, bus-driven
  invalidation on `session.status_changed(done)`, `file_diff`, `blame`.
- `core/prompts` — markdown templates with frontmatter + `{{var}}` rendering, defaults
  copied to `~/.maestro/prompts` on first run.
- `core/questions` — line-question flow: builds context (hunk + blame), renders the
  `line-question` template, dispatches to the active session or a fresh read-only one,
  owns the answer lifecycle (`question.answering` / `question.answered`).
- `core/gate` — `GateRule` trait + `GateRegistry` + `GateManager`; rules in
  `gate/rules.rs` with a span-tracking shell tokenizer.
- `core/attention` — the "who needs me?" queue, derived from bus events only; publishes
  `attention.updated`.
- `core/config` — `~/.maestro/config.toml`, written with commented defaults on first run
  and applied into the settings table at startup (one lookup path at runtime).
- `src/state/*` — zustand stores fed by bus events through `onBusEvent`.
- `sidecar/src` — `protocol.ts` (mirror of `core/agent/protocol.rs`, currently version 2),
  `engine.ts` (Claude Agent SDK, streaming input mode, runtime setters, dialog bridge),
  `models.ts` (session-independent model list), `mock.ts` (scripted, no API usage).

## Conventions and hard-won details

- **Never edit `src-tauri/` while the user is testing the app**: `tauri dev` rebuilds and
  restarts, which kills live sessions (they become `failed`). Frontend edits are safe —
  Vite HMR picks them up.
- **zustand 5 has no snapshot memoization.** A selector that builds a fresh value on
  every call (`?? []`, `.map(...)`, object literals) causes an infinite re-render loop.
  Selectors must return values already stored in state. This bit us twice (T6 review
  finding #1 and again in the fix round).
- **Green local checks are not a green build.** Two failures reached CI that no local
  command reproduced: a missing `icons/icon.png` (tauri's context generation needs a PNG
  on non-Windows) and clippy lints from a newer toolchain. The toolchain is pinned now;
  the icon lesson stands — the Linux build path differs from Windows.
- **Every automated check can pass while the feature is broken.** Both agent branches
  were green on cargo test/clippy/tsc/eslint/build and still crashed on mount. Run the
  app (mock mode) before calling anything done. A CI smoke test is a T10 item.
- Hotkeys use Alt (not Ctrl): the embedded CodeMirror editors and inputs own the Ctrl
  combinations. Alt+1…9 select a worktree, Alt+↑/↓ cycle, Alt+C/Alt+D switch panels.
- Mock sidecar keywords (`MAESTRO_SIDECAR_MOCK=1`): `PERMISSION` → chat permission
  prompt, `GATE` → push+PR command that the gate intercepts, `ASK` → agent question
  dialog (single-select + multi-select), `PLAN` → plan review, `AUTH` → MCP elicitation,
  `THINK` → streamed reasoning, `TOOLS` → tool call
  with a result, `SUBAGENT` → nested subagent activity, `TODO` → task checklist, `DENY` →
  auto-denied call, `LIMIT` → rate-limit warning, `CRASH` → kills the process to exercise
  supervisor recovery. Every turn also reports usage, and runtime model / effort / thinking
  / permission switches are echoed into the next reply — that is how the round trip is
  smoke-tested.
- **Read the SDK types, then verify against the CLI.** `AskUserQuestion` looks like it
  needs the `onUserDialog` dialog bridge (there is even a `permission_ask_user_question`
  dialog kind), and declaring it changes nothing. What actually happens with `canUseTool`
  set: the questions arrive as a **permission request** for tool `AskUserQuestion`, and the
  answers must ride back on the decision — `{behavior:"allow", updatedInput:{...input,
answers, annotations}}`, or `{behavior:"deny", message}` to reply instead. Allowing with
  an unchanged input is a dismissal, which is precisely what "granted permission, then
  nothing" was. Two probe scripts settled this in minutes; hours of reading did not.
- Dialog answers are shaped **in the sidecar**, never in the core or the UI: the pending
  entry carries its own `settle`, so one `DialogAnswer { answers, annotations, feedback }`
  from the UI resolves either a permission decision or a CLI dialog result. SDK shapes stay
  behind that boundary.
- `Options.supportedDialogKinds` is the real opt-in for CLI-raised dialogs — the callback
  alone receives nothing, and an undeclared kind is never emitted (the flow behind it
  degrades silently). It is deliberately empty until a kind has a renderer.
- The Rust toolchain is pinned in `rust-toolchain.toml` (1.97.1) so `clippy -D warnings`
  means the same thing locally and on the runner — a version gap once let four lints
  through to CI. rustup installs it on demand; CI reads the same file via `rustup show`.
  Bumping the version is a one-line change plus a full check run.
- `cargo test` needs `MAESTRO_SIDECAR_E2E=1` **and** a built sidecar for the e2e test;
  without the env var it is skipped (CI sets it and builds the sidecar first).
- Auth: the SDK uses the user's Claude Code OAuth login (no `ANTHROPIC_API_KEY` set), so
  sessions consume the subscription quota. Parallel agents burn it faster.
- Git identity ("Git identity"): local `user.name`/`user.email` are ESeverdev
  <egor.sievierin@gmail.com>, and the remote uses the SSH alias `github-maestro` with a
  dedicated key (`~/.ssh/id_ed25519_maestro`, `IdentitiesOnly yes`) so the work account
  can never push here. `gh`'s active account is **global**: switch accounts before running
  `gh` against a work repository.

## Fix rounds (what the reviews caught)

Findings and the full fix list live in `docs/tasks/T6-fixes.md` and
`docs/tasks/T7-fixes.md`. Highlights worth remembering:

- T7 gate was **fail-open** in normal use: a backslash-newline continuation fused
  tokens so `git push \`+newline+`--force` matched nothing. Matching is now fail-closed
  (basenames, wrapper words, grouping parens, nested `sh -c`/`eval`/`$()`), the registry
  prefers the most editable rule so `git push && gh pr create` still exposes PR fields,
  and ambiguous commands (repeated flags, several commits, file-sourced messages) gate
  with a `note` instead of misleading editable fields.
- Pending gates are cancelled when their session closes/crashes/is swept, and every
  departure publishes `gate.resolved` — otherwise a dead gate wedged the blocking modal.
- `bypassPermissions` is deliberately absent from the UI: it skips `canUseTool`, so the
  gate would never see a push.
- T6 answer attribution is core-driven: a queued question arms only when its own turn
  starts, so an unrelated turn's `awaiting_input` can no longer close it with the wrong
  text, and deltas are routed by `question_id` instead of "first unfinished per file".

## Verification commands

```sh
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm run lint && npm run typecheck && npm run format:check && npm test && npx vite build
cd sidecar && npm run build && npm run lint
```

Run the app without spending quota:

```powershell
$env:MAESTRO_SIDECAR_MOCK="1"; npm run tauri dev
```
