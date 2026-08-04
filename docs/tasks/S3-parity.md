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
- **Interactive dialogs, and why `AskUserQuestion` looked broken.** Two mechanisms exist
  and it matters which one a flow uses. Both were checked against the installed CLI, not
  just the typings — the typings alone point at the wrong one.
  - `Options.onUserDialog` + `Options.supportedDialogKinds` handle `request_user_dialog`
    control requests: `UserDialogRequest = {dialogKind, payload, toolUseID?}` answered with
    `{behavior:"completed", result}` or `{behavior:"cancelled"}`. The CLI **fails closed** on
    `supportedDialogKinds` — a kind that is not declared is never emitted and the flow
    behind it degrades to its no-dialog behaviour. Kinds in the CLI's registry:
    `permission_bash`, `permission_file`, `permission_powershell`, `permission_browser`,
    `permission_webfetch`, `permission_skill`, `permission_workflow`, `permission_monitor`,
    `permission_prompt`, `permission_enter_plan_mode`, `permission_exit_plan_mode_v2`
    (**plan approval, with the plan in the payload — the most valuable next one**),
    `permission_ask_user_question`, `refusal_fallback_prompt`, `auto_mode_flagged_allow`,
    `auto_mode_setup_review`, `fable_overage_consent_prompt`, `computer_use_approval`,
    `mcp_url_elicitation`, `chrome_install_setup`, `chrome_install_upsell`.
  - **`AskUserQuestion` does not use that path when `canUseTool` is set.** Verified twice
    with the real CLI (a minimal SDK probe and our own sidecar): declaring
    `permission_ask_user_question` changes nothing; the questions arrive as an ordinary
    `canUseTool` request for tool `AskUserQuestion` with the questions in the input, and the
    answers ride back **on the permission decision**:
    - answer → `{behavior:"allow", updatedInput:{...input, answers, annotations}}`
    - "wrong question, ask me differently" → `{behavior:"deny", message:"<text>"}`
      `answers` maps question text → chosen option label, comma-separated for multi-select;
      `annotations[question] = {preview?, notes?}`. Allowing **without** filling in `answers`
      is what produced the original symptom: the agent reported the question as dismissed.
      That is exactly what Maestro used to do (allow, empty input) — the permission dialog
      was the whole interaction the user ever saw.
- **Read-only session facts we never surface:** `getContextUsage()`, session cost,
  `accountInfo()`, `supportedModels()` (already used), `supportedCommands()` (already used),
  `supportedAgents()`, `mcpServerStatus()`.
- **Message types we currently drop on the floor** in `sidecar/src/engine.ts`: thinking
  deltas, tool _results_, subagent activity (we filter `parent_tool_use_id !== null`),
  `SDKPermissionDeniedMessage`, `SDKRateLimitEvent`, plan-mode output, task notifications.

## Tier 1 — DONE (2026-08-05)

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

### What shipped in Tier 1

- Protocol v2: `set_model` / `set_effort` / `set_permission_mode` / `user_dialog_response`
  requests, `user_dialog_request` event, and a `DialogAnswer` wire type
  (`{answers, annotations, feedback}`) that says nothing about how the CLI is answered.
- `SessionManager::{set_model, set_effort, set_permission_mode, respond_user_dialog}` —
  effort and mode validated against `EFFORT_LEVELS` / `PERMISSION_MODES`, a switch _into_ a
  writer mode re-checks the single-writer rule, every change persisted via
  `Store::set_session_runtime` and announced as `session.settings_changed`.
- Question bridge in the sidecar: a `canUseTool` request for `AskUserQuestion` becomes a
  `user_dialog_request` of kind `ask_user_question` (payload = the tool input), and the
  answer becomes the permission decision — allow with `answers`/`annotations` merged into
  `updatedInput`, deny with the user's own text when they reply instead. Resolution is
  published as `session.user_dialog_resolved`, which clears the attention entry
  (`AttentionKind::Question`).
- `onUserDialog` is wired for CLI-raised dialogs as well, with a 5-minute auto-cancel and a
  fail-closed kind check; `SUPPORTED_DIALOG_KINDS` is empty until a kind has a renderer, so
  no flow can hang waiting for a dialog Maestro would not draw.
- UI: `RuntimeControls` in the session toolbar (model list shows `display_name — id`),
  `QuestionDialog` (multi-question, multi-select, per-option preview, "Other" free text,
  per-question notes, "reply instead" → deny-with-message), answers and switches recorded
  in the transcript, and `/model`, `/effort`, `/permissions` local commands whose
  **argument** is autocompleted — which is where model ids become discoverable in the chat.
- Mock mode: `ASK` raises an `ask_user_question` payload with a single-select and a
  multi-select question; runtime switches are echoed into the next reply.
- Verified end to end against the real SDK (not only mock): the dialog is raised, the
  answer reaches the agent ("You chose **Blue**"), and all three setters are accepted
  mid-session.

Known limits carried forward: `SUPPORTED_DIALOG_KINDS` is empty, so plan approval
(`permission_exit_plan_mode_v2`) still degrades to its no-dialog behaviour; image
attachments in answers (`contentBlocks`) are not supported.

## Tier 2 — DONE (2026-08-05)

4. **Thinking blocks.** Streamed as `session.thinking_delta` and rendered folded, apart
   from the answer. Two findings, both from probing the real CLI:
   - With the CLI's default thinking config the tested models produced **no thinking at
     all**, so there was nothing to show. Thinking is therefore a session knob now
     (`default` / `off` / 4k / 16k / 32k), at spawn and at runtime
     (`Query.setMaxThinkingTokens`), persisted like model and effort (migration 5).
   - A budget alone is not enough: without `display: "summarized"` the assistant message
     carries a thinking block whose content is **empty**. With it, reasoning arrives as
     deltas. Both the spawn config and the runtime setter pass it.
     The assistant message's thinking block is also emitted as a fallback when no deltas
     streamed, so nothing is lost if a future CLI stops streaming them.
5. **Tool results.** `tool_use` now carries `tool_use_id`, and `session.tool_result`
   (matched by that id) fills in the entry, which renders running / done / error with the
   output folded inside. Output is truncated at 4 000 characters in the sidecar.
6. **Subagent activity.** Messages with `parent_tool_use_id` are no longer dropped: text,
   thinking and tool calls from a subagent are nested under the `Task` entry that spawned
   it, with a count badge on the summary line.
7. **Plan / checklist.** This CLI has **no `TodoWrite`** — work is tracked as tasks
   (`task_started` / `task_updated` / `task_notification` system messages). The sidecar
   folds those into a list and republishes it whole as `session.todos`; `TodoWrite` input
   is still parsed for older CLIs. Rendered above the input as a checklist.
8. **Cost, context, rate limits.** `session.usage` carries turn totals (cost, turns,
   tokens) plus a context reading fetched with `getContextUsage()` after every turn — a
   control request, so it costs nothing. The session toolbar shows `$cost` and a context
   bar that turns amber past 80%. `session.rate_limit` drives a pill in the panel header
   and, when the status is not `allowed`, an `error.raised` warning so it also toasts.

Also brought forward from Tier 3, because it was one event away: **auto-denied tool calls**
(item 9) arrive as `session.permission_denied` and render as a muted-red transcript entry
with the deciding reason — previously an agent that got refused by the classifier just
appeared to skip work.

Verified against the real CLI: tool calls and results, nested subagent activity, a growing
task checklist, cost $1.05 / context 4% of 1 000 000 tokens on one run, and 867 characters
of streamed reasoning once a budget and summarized display were set.

## Tier 3 — completeness, lower urgency

9. ~~**Auto-denied tool calls**~~ — done in Tier 2 (`session.permission_denied`).
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
