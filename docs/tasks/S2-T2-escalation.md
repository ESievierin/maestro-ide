# S2-T2 — `ask_original_agent` escalation tool

You are implementing task S2-T2 of MaestroIDE inside a dedicated git worktree. Work only
in this worktree. Commit your work at the end; never push.

This is the architecturally sensitive task of Stage 2 — hold it to the standard of the
sidecar/session work (T3) and the gate (T7): traits on boundaries, every outcome an event,
structured fallbacks instead of errors that kill a turn, thorough tests.

## Project context

MaestroIDE orchestrates parallel Claude Code agents on git worktrees. Read `docs/STATUS.md`
first (architecture map, conventions, verification commands), then the Stage 1 spec
`maestro-stage1-prompt.md`.

**Stage 2 goal:** an agent answering PR review comments can reach the reasoning of the
agent that implemented the work. `TASK_NOTES.md` (built in parallel as S2-T1) is the
primary channel. This task builds the escalation channel: a custom tool the review agent
can call to ask the _original implementation session_ a pointed question, read-only, on a
cheap model.

What exists and must be reused, not rebuilt:

- `sidecar/src/engine.ts` — wraps the Claude Agent SDK `query()` in streaming-input mode,
  one `AgentSession` per Maestro session, `canUseTool` bridged to the core as
  `permission_request`. `sidecar/src/protocol.ts` mirrors
  `src-tauri/src/core/agent/protocol.rs` (NDJSON over stdio, `PROTOCOL_VERSION = 1`).
  `sidecar/src/mock.ts` is a scripted engine used by tests and manual UI runs
  (`MAESTRO_SIDECAR_MOCK=1`, keywords `PERMISSION`, `GATE`, `CRASH`).
- `core/agent` — `AgentEngine` trait + `SidecarEngine` supervisor (request/ack
  correlation, crash detection, lazy restart).
- `core/session` — `Session` entity (`sdk_session_id` persisted for resume), state
  machine, `SessionManager::spawn(SpawnParams)` (params include `resume_from`, which
  resolves a stored `sdk_session_id`), single-writer rule (read-only = `plan` mode
  sessions are not writers).
- `core/gate` — `GateRegistry`/`GateRule`/`GateManager`; a matched tool call publishes
  `gate.pending` and waits for the user instead of executing.
- `core/attention` — the "needs you" queue, derived from bus events.

## Decisions already made — implement these, do not re-litigate

1. **Escalation target:** the latest `implementation`-type session of the asking session's
   branch that has a stored `sdk_session_id`, by `created_at`. Never research/manual, never
   fan-out to several.
2. **The escalated session is read-only and cheap:** resumed with `permission_mode: "plan"`,
   `Edit`/`Write`/`Bash` disallowed, and **model forced to `sonnet`** regardless of what
   the original ran on.
3. **Budget: at most 2 escalations per asking turn**, enforced in core (count per asking
   session, reset when that session starts a new turn — its `streaming` transition), never
   by prompt wording alone.
4. **Every failure is a structured tool result, never a thrown error**: no implementation
   session, resume failure, sidecar restart mid-flight, timeout, over budget → a result the
   asking agent can read and continue from ("context unavailable — answer from notes and
   code").
5. **Protocol goes to v2. Compatibility rule: ignore-unknown for events, explicit nack for
   requests.** The Rust reader already logs and skips unparseable event lines — keep that
   and document it as the rule. The sidecar already nacks unknown request types — keep it,
   and additionally: when `ready` reports a protocol version lower than the core's, publish
   an `error.raised` (severity error) saying the sidecar is stale and needs rebuilding.
   Today a stale `sidecar/dist` silently lacks features; that must be visible.
6. **New session type `escalation`.** `sessions.type` is a TEXT column, so extending the
   enum needs no migration. `tools_profile` **does** need persisting (audit + resume), so
   add exactly one migration appending a `tools_profile TEXT` column to `sessions`.
7. **Escalation sessions are excluded from the attention panel and the single-writer rule.**
   Read-only mode already excludes them from single-writer; assert it in a test. The
   attention queue must not enqueue `session_failed` items for them.

## Implementation plan

### 1. Sidecar: the custom tool

- Spawn requests gain `tools_profile?: string` (extensible — not a boolean). Profile
  `"review"` registers the tool; absent/unknown profile registers nothing.
- Register `ask_original_agent({ question: string })` as an SDK in-process MCP tool
  (`createSdkMcpServer` + `tool` from `@anthropic-ai/claude-agent-sdk`; check the exact
  API in `node_modules/@anthropic-ai/claude-agent-sdk/sdk.d.ts` — do not guess, the SDK
  types are the authority). The tool handler must:
  1. emit `escalation_request { session_id, request_id, question }`,
  2. await the matching `escalation_response { request_id, result }`,
  3. return `result` as the tool's text content.
- Correlate by `request_id` in a map, exactly as `pendingPermissions` does in
  `engine.ts`. On session close, resolve anything outstanding with a "session closed"
  result so the query can unwind.
- `PROTOCOL_VERSION = 2` in both `protocol.ts` and `protocol.rs`.
- **Mock engine:** keyword `ESCALATE` in a prompt triggers one scripted escalation
  round-trip (emit `escalation_request`, wait for the response, stream it back as the
  answer text), so the whole path is testable without the SDK.

### 2. `core/escalation`

`src-tauri/src/core/escalation/mod.rs` — `EscalationManager`:

- Depends on `Arc<dyn AgentEngine>`, `Arc<dyn Store>`, `Arc<SessionManager>`, the bus.
- `handle_request(session_id, request_id, question)`:
  1. Resolve the asking session's branch → latest `implementation` session with a
     `sdk_session_id`.
  2. Budget check (see decision 3) → over budget: structured refusal result.
  3. Spawn the escalated session through `SessionManager::spawn` with
     `session_type: Escalation`, `resume_from: <that session id>`,
     `permission_mode: "plan"`, `model: "sonnet"`, `tools_profile: None`,
     `disallowed_tools: ["Edit", "Write", "Bash"]` (add a field to `SpawnParams` and to the
     spawn protocol request; the sidecar passes it to the SDK's `disallowedTools`).
  4. Collect the escalated session's final answer text: subscribe to the bus for its
     `session.stream_delta` and complete on its `awaiting_input`/terminal transition
     (mirror how `core/questions` arms and completes a line question — same problem, same
     shape; reuse the pattern, do not copy-paste blindly).
  5. Close the escalated session, send `escalation_response` with the answer.
  6. Timeout: setting `escalation_timeout_secs`, default 120 → interrupt the escalated
     session and return the structured timeout result.
- Publish `escalation.started { asking_session_id, target_session_id, question }`,
  `escalation.finished { .., chars }`, `escalation.failed { .., reason }` (append to
  `core/bus/mod.rs`).
- Route `SidecarEvent::EscalationRequest` from the session manager's event handling into
  this manager (same shape as the gate hook in the `PermissionRequest` arm: a small early
  dispatch, nothing restructured).

### 3. Safety: the gate applies to escalated sessions too

A gated tool call arriving from an `escalation`-type session must be **hard-denied
immediately** (respond deny with a message saying escalated sessions cannot act), not
surfaced to the user as a `gate.pending`. Defense in depth: tools are already disallowed,
so reaching this path means something upstream failed.

### 4. Tests

- Budget: third call in one turn refuses without spawning; a new `streaming` transition on
  the asking session resets the count.
- Fallbacks: branch has no implementation session; the target has no `sdk_session_id`;
  spawn returns an error; sidecar crash mid-escalation → each yields the structured result
  and an `escalation.failed` event.
- Timeout: with a 1s timeout and a target that never answers → interrupt + timeout result.
- Read-only: an escalation session does not take the branch's writer slot (spawn a writer
  after it and assert it is still a writer); the attention queue ignores its failure.
- Gate: `git push` from an escalation session → engine received a deny, no `gate.pending`.
- Protocol: a v1-shaped event line still parses; an unknown event type is skipped with a
  warning rather than killing the reader; the sidecar nacks an unknown request type.
- Mock e2e (extend `core::agent::tests::sidecar_mock_end_to_end` or add a sibling behind
  `MAESTRO_SIDECAR_E2E=1`): an `ESCALATE` prompt round-trips a real
  `escalation_request`/`escalation_response` pair through the real Node process.

## Checks (all must pass)

```
cd sidecar && npm install && npm run build && npm run lint && cd ..
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && MAESTRO_SIDECAR_E2E=1 cargo test
cd .. && npm install && npm run lint && npm run typecheck && npx prettier --check src && npm test && npx vite build
```

**Then run the app in mock mode and exercise the path** — green checks have twice hidden a
broken app:

```powershell
$env:MAESTRO_SIDECAR_MOCK="1"; npm run tauri dev
```

Start a session with a prompt containing `ESCALATE`, confirm the escalation events appear
in the event log panel and the answer comes back in the chat, no console errors. Stop the
app afterwards.

## Conflict avoidance (S2-T1 runs in a parallel worktree)

- Do not touch: `core/notes/*` (does not exist — do not create), `core/prompts/*`,
  `core/diff/*`, `core/questions/*`, `src/views/DiffViewer.tsx`, `src/views/NotesPanel.tsx`,
  `src/state/notes.ts`, `prompts-defaults/*`, `.github/*`.
- `core/session/manager.rs` is shared: you own `spawn()`/`SpawnParams`/`handle_event` and
  the session-type plumbing. **Do not touch `close()`** — the parallel task owns it.
- `core/session/mod.rs` is yours (enum + `tools_profile`).
- In shared files (`ipc/mod.rs`, `lib.rs`, `core/mod.rs`, `core/bus/mod.rs`,
  `src/types/events.ts`, `src/styles.css`) only append at the end of the relevant section;
  never reorder existing entries.
