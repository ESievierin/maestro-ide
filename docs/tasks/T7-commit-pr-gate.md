# T7 — Gate: registry + commit/PR approval flow

You are implementing task T7 of MaestroIDE inside a dedicated git worktree. Work only in
this worktree. Do not run `npm run tauri dev`. Commit your work at the end (small logical
commits are fine); never push.

## Project context

MaestroIDE is a Tauri 2 desktop app orchestrating parallel Claude Code agents on git
worktrees. Read `README.md` and `maestro-stage1-prompt.md` at the repo root first.

Non-negotiable architecture rules (from the project brief):

- All logic lives in the Rust core (`src-tauri/src/core/*`). The frontend only renders
  state and sends commands.
- Every state change is an event on the central bus (`core/bus`); UI panels subscribe.
- **Registries, not switch statements**: gated operations register matchers + handlers
  in a gate registry. Adding a new gated action must not require touching core dispatch
  code.
- Typed errors (`src-tauri/src/error.rs`), no `unwrap()` in core paths, `tracing` logs.

How agent tool permissions flow today (this is the mechanism you hook into):

- The Node sidecar wraps the Claude Agent SDK. When the agent wants to run a tool, the
  sidecar emits a `permission_request { session_id, request_id, tool, args, title }`
  protocol event (see `sidecar/src/protocol.ts` — you do NOT need to change the sidecar).
- `core/session/manager.rs` receives it as `SidecarEvent::PermissionRequest` in
  `handle_event` and currently forwards it to the UI as the bus event
  `session.permission_request` (rendered as Allow/Deny in the chat panel).
- The decision goes back via `SessionManager::respond_permission(request_id, allow,
updated_args, message)` → sidecar → SDK `canUseTool` result. `updated_args` replaces
  the tool input; `message` is the deny feedback shown to the agent.

## Task (from the Stage 1 brief)

Gate registry: matchers on (tool, args pattern) → handler. Register: `git push`,
`gh pr create`, and optionally `git commit` (config setting). A matching tool call
pauses → `gate.pending` event with extracted params (commit message / PR title+body) →
GateDialog with editable fields → allow (with edited params substituted into the tool
args) / deny (with optional feedback text returned to the agent). Nothing matched by
the registry ever executes without explicit approval.

DoD: agent prepares a commit and PR; the user edits the message/title in the dialog;
only the approved content is executed.

## Implementation plan

### 1. `core/gate` module

- `src-tauri/src/core/gate/mod.rs` (+ `rules.rs` if you want):
  - `GateMatch { kind: String, params: Vec<GateParam> }` where
    `GateParam { key: String, label: String, value: String, multiline: bool }` —
    e.g. commit → one param `message`; pr → `title` + `body`.
  - `trait GateRule: Send + Sync { fn id(&self) -> &str; fn matches(&self, tool: &str,
args: &serde_json::Value) -> Option<GateMatch>; fn apply(&self, args:
&serde_json::Value, edited: &[GateParam]) -> serde_json::Value; }` — `apply`
    substitutes the (possibly edited) params back into the tool args and returns the
    updated args for `respond_permission`.
  - `GateRegistry { rules: Vec<Box<dyn GateRule>> }` with `register(rule)` and
    `match_tool(tool, args)`. Registration happens once at startup in `lib.rs` —
    adding a new gated action means writing a new rule and one `register` call, nothing
    else.
  - Built-in rules (all match the `Bash` tool; parse `args["command"]` as a string):
    - `git_push`: command contains a `git push` invocation (word-boundary match; also
      match `git -C <path> push`). No editable params (params list may be empty — the
      dialog then just shows the raw command with Allow/Deny).
    - `gh_pr_create`: `gh pr create …` — extract `--title "…"`/`-t` and `--body "…"`/
      `-b` values (support both `--flag value` and `--flag=value`, single/double
      quotes). Editable params title (single-line) + body (multiline). `apply` must
      rebuild the command with the edited values properly quoted.
    - `git_commit`: `git commit …` — extract `-m "…"`/`--message`. Registered only when
      the settings key `gate_commit` is `"true"` (default off; read via
      `Store::get_setting` at startup).
  - Command-string parsing: write a small shell-aware tokenizer (handle single and
    double quotes; you do not need full POSIX fidelity — cover the common quoting the
    agent CLI produces). Thorough unit tests for the matchers and `apply` rebuilding,
    including quotes inside messages.
  - `PendingGates`: map gate_id (uuid) → `{ request_id, session_id, branch, rule_id,
kind, params, original_args, created_at }`, with `list()` for UI reload.
- Wire into the session manager with **minimal surface**: in
  `core/session/manager.rs`, `SessionManager` gets an
  `Option<Arc<GateManager>>` (or a trait object) set at construction; in `handle_event`
  for `PermissionRequest`, first try the gate: if it matches, record the pending gate
  and publish the bus event `gate.pending` (extend the existing `Event::GatePending`
  payload in `core/bus/mod.rs` with `kind`, `params`, `branch`, `tool`, `raw_args` —
  keep the event name); otherwise keep the existing `session.permission_request`
  behavior unchanged. The manager needs the session's branch — it already tracks it in
  its runtime map.
- `respond_gate(gate_id, allow, edited_params, feedback)` on the gate manager: look up
  the pending entry, on allow → `rule.apply(original_args, edited_params)` →
  `SessionManager::respond_permission(request_id, true, Some(updated_args), None)`;
  on deny → `respond_permission(request_id, false, None, feedback)`. Publish an
  `attention.required` clear? No — just remove the pending entry. Handle unknown
  gate_id with a typed error.
- Unit tests: matcher extraction (`gh pr create --title "A \"quoted\" title" --body
'multi word'`), apply-rebuild round-trip, registry routing (non-matching tool falls
  through), manager integration with the existing MockEngine pattern (permission
  request for `git push` → `gate.pending` published, respond allow → engine received
  `respond_permission` with updated args; git commit rule respects the `gate_commit`
  setting).

### 2. IPC + wiring

- Commands (append at the end of `ipc/mod.rs`, register in `lib.rs`):
  - `list_pending_gates() -> Vec<PendingGateInfo>`
  - `respond_gate(gate_id, allow, edited_params: Vec<GateParam>, feedback: Option<String>)`
- Construct the registry + gate manager in `lib.rs` before `AppState`, pass into
  `SessionManager`, add to `AppState`.

### 3. Frontend: GateDialog

- `src/state/gates.ts`: zustand store of pending gates; fed by `gate.pending` bus
  events (`onBusEvent`) + initial `list_pending_gates` fetch on startup; `respond`
  action invoking `respond_gate` and removing the entry.
- `src/views/GateDialog.tsx`: global modal rendered from `App.tsx` whenever pending
  gates exist (queue: show the oldest first, with a "N more pending" hint). Content:
  session/branch line, the kind (e.g. "git push", "PR creation", "commit"), the raw
  command in a `<code>` block, editable inputs for each param (`multiline` → textarea),
  Allow (uses edited values) / Deny with an optional feedback text input.
- Reuse the existing modal styles (`.modal-backdrop`, `.modal`, `.form-grid`,
  `.modal-actions` in `src/styles.css`); append any new styles at the end of the file
  in a commented section.
- Add the `gate.pending` payload to `src/types/events.ts` (append-only edit).

### 4. Checks (all must pass)

```
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm install && npm run lint && npm run typecheck && npx prettier --check src && npx vite build
```

(`npm install` is needed once — worktrees don't inherit node_modules. Do NOT touch
`sidecar/`.)

## Conflict avoidance (T6 runs in a parallel worktree)

- Do not modify: `core/diff/*`, `core/prompts/*` (doesn't exist — don't create it),
  `core/questions/*` (same), `src/views/DiffViewer.tsx`, `src/state/diffs.ts`,
  `sidecar/*`, `.github/*`, `prompts-defaults/*`.
- In shared files (`ipc/mod.rs`, `lib.rs`, `core/mod.rs`, `core/bus/mod.rs`,
  `src/types/events.ts`, `src/App.tsx`, `src/styles.css`): only append new items at
  the end of the relevant sections; never reformat or reorder existing code.
- `core/session/manager.rs` is yours to touch, but keep the change minimal (the gate
  hook in `PermissionRequest` handling + constructor parameter); do not restructure
  the file.
