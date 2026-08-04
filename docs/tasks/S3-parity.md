# S3 — Claude Code parity inside Maestro sessions

Goal: a session in Maestro should not be a weaker Claude Code than the terminal one. Today
it is: several capabilities the CLI exposes are simply not wired through our sidecar, so the
agent either cannot use them or silently hangs.

This document is the inventory. It is ordered by what actually blocks work, and every item
names the SDK surface it needs, so implementation is mechanical rather than exploratory.

## What the investigation established (facts, from the installed SDK types and bundle)

- **Runtime control exists on the `Query` object** — we simply never call it:
  - `setModel(model?)` — change model mid-session.
  - `applyFlagSettings({ effortLevel })` — change effort mid-session (`Settings.effortLevel`
    is the effort knob; the mapped-flag type calls it out explicitly).
  - `setPermissionMode(mode)` — change permission mode mid-session.
  - `setMaxThinkingTokens(n)` — thinking budget (deprecated in favour of `thinking`, ignore).
- **Interactive dialogs (this is why `AskUserQuestion` looks broken).** The CLI asks the host
  to render a blocking dialog via a `request_user_dialog` control request, delivered to the
  `onUserDialog?: (request, {signal}) => Promise<UserDialogResult>` option.
  `UserDialogRequest = { dialogKind: string, payload: Record<string, unknown>, toolUseID? }`;
  the answer is `{behavior: "completed", result}` or `{behavior: "cancelled"}`.
  `dialogKind` is an **open union** — hosts must answer unknown kinds with `cancelled`.
  We do not pass `onUserDialog` at all, and the SDK bundle then, in its own words, stays
  "silent so a capable client (or the worker's park deadline) settles it" — i.e. the tool
  call hangs forever. That is exactly the observed "permission granted, then nothing".
  For the question dialog the result shape mirrors `AskUserQuestionOutput`:
  `{ questions, answers: { [question]: string }, response?: string, annotations? }`
  (multi-select answers are comma-separated; `response` is freeform text the user typed
  instead of picking an option).
- **Read-only session facts we never surface:** `getContextUsage()`, session cost,
  `accountInfo()`, `supportedModels()` (already used), `supportedCommands()` (already used),
  `supportedAgents()`, `mcpServerStatus()`.
- **Message types we currently drop on the floor** in `sidecar/src/engine.ts`: thinking
  deltas, tool _results_, subagent activity (we filter `parent_tool_use_id !== null`),
  `SDKPermissionDeniedMessage`, `SDKRateLimitEvent`, plan-mode output, task notifications.

## Tier 1 — blocking, implement first

1. **Runtime model / effort / permission-mode switching.** New sidecar requests
   (`set_model`, `set_effort`, `set_permission_mode`) → the `Query` setters above; core
   persists the new value on the session row so the UI and history stay truthful; the session
   toolbar gets the three selectors (not just the spawn form). Note for the permission mode:
   switching _into_ `auto` carries the same gate caveat as spawning with it.
2. **User-dialog bridge → `AskUserQuestion` works.** Pass `onUserDialog`; forward the request
   over the protocol (`user_dialog_request {session_id, request_id, dialog_kind, payload}` /
   `user_dialog_response {request_id, behavior, result}`); render the question dialog in the
   UI (1–4 questions, 2–4 options each, optional multi-select, optional per-option preview,
   plus an "Other" free-text field, per the payload shape); answer unknown `dialog_kind`s
   with `cancelled` so nothing can hang. A timeout that auto-cancels is required — a hung
   dialog blocks the agent's turn.
3. **Model names in the UI are discoverable.** The dynamic list from `supportedModels()`
   already exists; extend it: show ids next to display names, keep the list in the session
   toolbar for switching, and add `/model <name>` handling to the local-command layer so the
   chat input can switch models too (the CLI's own `/model` is interactive and does not work
   through the SDK path).

## Tier 2 — visible gaps in the transcript

4. **Thinking blocks.** `stream_event` carries thinking deltas; render them collapsed
   ("thinking…" with an expandable body), separate from answer text.
5. **Tool results.** We show `tool_use` but never the result; a session where the agent reads
   files looks like it did nothing. Forward the `user` message's `tool_use_result` /
   `tool_result` content, matched to its `tool_use_id`, and render it inside the existing
   collapsible tool entry.
6. **Subagent activity.** Messages with `parent_tool_use_id` are dropped, so a `Task`/agent
   call is invisible. Nest them under the spawning tool entry (`subagent_type`,
   `task_description` are on the message).
7. **TodoWrite / plan rendering.** The agent's todo list and plan-mode output are structured
   data; render them as a checklist instead of raw JSON in a tool entry.
8. **Cost, context and rate limits.** `result` messages carry `total_cost_usd`, `num_turns`,
   `usage`; `getContextUsage()` gives context pressure; `rate_limit_event` warns before a
   wall. Show per-session cost + a context meter in the session toolbar.

## Tier 3 — completeness, lower urgency

9. **Auto-denied tool calls** (`SDKPermissionDenied`) — currently invisible; render as a
   denied tool entry so `dontAsk`/`auto` denials are explicable.
10. **File rewind / checkpoints** (`rewindFiles`) — the CLI's `/rewind`; needs a UI to pick a
    message to rewind to.
11. **MCP servers** — `mcpServerStatus`, `toggleMcpServer`, `reconnectMcpServer`; the session
    already inherits the user's MCP config, but nothing is visible or controllable.
12. **Agents** — `supportedAgents()` and the `agent` spawn option (run a session as a named
    subagent profile).
13. **Elicitation** (`onElicitation`) — MCP servers asking for input/auth; same bridge as the
    user dialog, different callback.
14. **Attachments** — pasting images into the chat input (`SDKUserMessage` content blocks
    support images).

## Explicitly out of scope

Terminal-only affordances that make no sense here (IDE integrations, `/vim`, status line,
output styles), and anything Stage 2/3 owns (escalation, notes, GitHub polling).

## Verification for every item

Beyond the standard checks (`cargo fmt/clippy/test`, `lint/typecheck/test/build`, sidecar
build/lint): each item needs a mock-mode path so it can be exercised without burning quota,
and a real-SDK smoke run before it counts as done. The mock sidecar already has
`PERMISSION` / `GATE` / `CRASH` / `ESCALATE`-style keywords — extend that vocabulary
(`ASK` for the question dialog, `THINK` for thinking blocks, and so on).
