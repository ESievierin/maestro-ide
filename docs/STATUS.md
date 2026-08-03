# MaestroIDE — project status snapshot

Living handoff document: where Stage 1 stands, what the moving parts are, and the
conventions that are easy to lose between sessions. Update it when a task lands.

Last updated: 2026-08-03 (after T6+T7 merge and fix round).

## Stage 1 progress

| Task                       | State                                                                  |
| -------------------------- | ---------------------------------------------------------------------- |
| T1 skeleton                | done — bus, IPC bridge, SQLite+migrations, typed errors, tracing, CI   |
| T2 GitProvider + worktrees | done — trait + git CLI impl, worktree manager, WorktreeList UI         |
| T3 sidecar + sessions      | done — NDJSON protocol v1, supervisor, session state machine           |
| T4 chat panel              | done — markdown, tool-use folding, single-writer, commands, resume     |
| T5 diff engine + viewer    | done — snapshot cache, CM6 split/unified, worktree & committed scopes  |
| T6 line questions          | done (agent-built, reviewed, fixed) — see "Fix rounds"                 |
| T7 commit/PR gate          | done (agent-built, reviewed, fixed) — see "Fix rounds"                 |
| T8 prompt templates        | next — `core/prompts` exists from T6; needs defaults + PromptEditor UI |
| T9 attention panel         | not started                                                            |
| T10 polish / dogfood       | not started                                                            |

Branch `master` holds everything; `impl/T-6-string-questions` and
`impl/T-7-commit-pr-gate` are merged and can be deleted once their worktrees are gone.

## Architecture map (where things live)

- `src-tauri/src/core/bus` — typed `Event` enum, tokio broadcast. Every state change is
  an event; UI panels and future daemon subscribe. Frontend receives them on the single
  Tauri channel `maestro:event` (`ipc::spawn_event_forwarder`).
- `core/store` — `Store` trait + SQLite impl, migrations append-only in
  `store/migrations.rs` (currently 5). Branch name is the primary key.
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
- `src/state/*` — zustand stores fed by bus events through `onBusEvent`.
- `sidecar/src` — `protocol.ts` (mirror of `core/agent/protocol.rs`), `engine.ts`
  (Claude Agent SDK, streaming input mode), `mock.ts` (scripted, no API usage).

## Conventions and hard-won details

- **Never edit `src-tauri/` while the user is testing the app**: `tauri dev` rebuilds and
  restarts, which kills live sessions (they become `failed`). Frontend edits are safe —
  Vite HMR picks them up.
- **zustand 5 has no snapshot memoization.** A selector that builds a fresh value on
  every call (`?? []`, `.map(...)`, object literals) causes an infinite re-render loop.
  Selectors must return values already stored in state. This bit us twice (T6 review
  finding #1 and again in the fix round).
- **Every automated check can pass while the feature is broken.** Both agent branches
  were green on cargo test/clippy/tsc/eslint/build and still crashed on mount. Run the
  app (mock mode) before calling anything done. A CI smoke test is a T10 item.
- Mock sidecar keywords (`MAESTRO_SIDECAR_MOCK=1`): `PERMISSION` → chat permission
  prompt, `GATE` → push+PR command that the gate intercepts, `CRASH` → kills the
  process to exercise supervisor recovery.
- Rust toolchain here is 1.89, so `rusqlite 0.39` + `rusqlite_migration =2.5.0` are
  pinned (2.6 needs rustc 1.95). Unpin after `rustup update`.
- `cargo test` needs `MAESTRO_SIDECAR_E2E=1` **and** a built sidecar for the e2e test;
  without the env var it is skipped (CI sets it and builds the sidecar first).
- Auth: the SDK uses the user's Claude Code OAuth login (no `ANTHROPIC_API_KEY` set), so
  sessions consume the subscription quota. Parallel agents burn it faster.
- Git identity is set **locally** to ESeverdev <egor.sievierin@gmail.com>. A separate
  GitHub account is still to be created; no remote is configured and nothing has been
  pushed. Set up SSH/HTTPS auth separation before the first push, and `gh auth` before
  using the PR gate for real.

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
cd .. && npm run lint && npm run typecheck && npm run format:check && npx vite build
cd sidecar && npm run build && npm run lint
```

Run the app without spending quota:

```powershell
$env:MAESTRO_SIDECAR_MOCK="1"; npm run tauri dev
```
