# MaestroIDE — Stage 1 Implementation Brief

You are implementing **MaestroIDE**: a Tauri desktop app for orchestrating parallel agentic development on top of git worktrees and Claude Code (via the Claude Agent SDK). The central entity is a task/worktree, not a file. The user manages a fleet of agents: assigns tasks, reviews diffs, gates commits/PRs, and sees at a glance which agent needs attention.

Work through the tasks below **in order** (T1 → T10), respecting the dependency graph. After completing each task, stop and summarize what was built, list any deviations from this spec with reasons, and wait for my confirmation before starting the next task.

---

## Non-negotiable engineering principles

These apply from the first commit. Stage 2–4 features (background daemon, cross-agent context escalation, rebase-all automation) will be added later — the architecture must absorb them without rewrites.

1. **Layered architecture, thin UI.** All logic lives in the core layer (Rust). The frontend only renders state and sends commands. No business logic in components.
2. **Event-driven core.** Every state change is an event on a central event bus: `worktree.created`, `worktree.removed`, `session.status_changed`, `session.stream_delta`, `diff.updated`, `gate.pending`, `attention.required`, `error.raised`. UI panels, notifications, and the future daemon are subscribers. Never call UI update logic directly from core modules.
3. **Interfaces (traits) on every external boundary:** `GitProvider`, `AgentEngine`, `PrProvider`, `Store`. Concrete impls: git CLI wrapper, Node sidecar bridge, `gh` CLI wrapper, SQLite. All core logic depends on traits, enabling test doubles and future GitLab/other-SDK support.
4. **Session is a first-class entity** with: id, branch, type (`research` | `implementation` | `review_fix` | `manual` — extensible enum), status state machine (`spawning → streaming → awaiting_input → done | failed | cancelled`), model, effort, created_at. Every session is bound to a branch at spawn time and persisted.
5. **Registries, not switch statements.** Gated operations (commit, push, pr-create) register matchers + handlers in a gate registry. Adding a new gated action must not require touching core dispatch code.
6. **Prompts are data.** All prompt templates live as markdown files with frontmatter in `~/.maestro/prompts/`, rendered through one template engine (`{{var}}` substitution). New prompt type = new file, zero code changes.
7. **SQLite with migrations from day one.** Schema will grow every stage. Use a migration framework even while there are only two tables.
8. **Typed errors, surfaced as events.** Agent operations fail routinely (rate limits, network, cancellation) — these are normal states with severity levels, not panics. No `unwrap()` in core paths.
9. **Structured logging from day one** (tracing crate): session ids, branch, event names in fields. Debugging parallel agents without this is impossible.
10. **Code quality:** rustfmt + clippy (deny warnings) for Rust, eslint + prettier + strict TypeScript for frontend. Small modules, no god-objects. Public APIs of core modules documented. Unit tests for core logic (state machines, gate matching, template rendering, store); integration tests where cheap.

## Stack

- **Shell:** Tauri 2.x, Rust backend.
- **Frontend:** TypeScript, React, zustand for state, CodeMirror 6 for diff rendering. Tailwind ok.
- **Agent engine:** Node.js sidecar process embedding the **Claude Agent SDK** (TypeScript). Rust core owns all state; sidecar only executes sessions. Communication over stdio with newline-delimited JSON messages (protocol defined in T3). The sidecar is launched/supervised by the Rust core as a Tauri sidecar binary.
- **Persistence:** SQLite (rusqlite or sqlx) + migrations. Store keyed by **branch name** — branch is the primary key linking worktree ↔ task ↔ PR. Worktrees are disposable; branch state survives worktree re-creation.
- **Git:** wrap the `git` CLI (not libgit2) — worktree support and behavior must match user-facing git. PRs via `gh` CLI.

## Repository layout

```
maestro/
├─ src-tauri/                # Rust core
│  └─ src/
│     ├─ core/
│     │  ├─ bus/             # event bus
│     │  ├─ worktree/        # GitProvider trait + git CLI impl, worktree manager
│     │  ├─ session/         # session entity, state machine, registry
│     │  ├─ agent/           # AgentEngine trait + sidecar bridge
│     │  ├─ diff/            # diff computation, changed files, blame
│     │  ├─ gate/            # gate registry, pending-approval flow
│     │  ├─ prompts/         # template loading + rendering
│     │  └─ store/           # SQLite, migrations
│     ├─ ipc/                # Tauri commands + event forwarding to frontend
│     └─ error.rs            # typed error hierarchy
├─ sidecar/                  # Node + Claude Agent SDK
│  └─ src/ (protocol.ts, engine.ts, main.ts)
├─ src/                      # frontend
│  ├─ state/                 # zustand stores fed by Tauri events
│  ├─ views/                 # WorktreeList, DiffViewer, ChatPanel,
│  │                         # AttentionPanel, GateDialog, PromptEditor
│  └─ components/
└─ prompts-defaults/         # default templates copied to ~/.maestro/prompts on first run
```

---

## Tasks

### T1 — Project skeleton
Scaffold Tauri app + React frontend + Node sidecar package. Implement:
- Event bus in core (typed event enum, subscribe/publish; async, tokio-based).
- IPC bridge: Tauri commands for frontend→core; core events forwarded to frontend via Tauri events; frontend zustand store subscribing to them.
- SQLite store with migration runner; initial migration: `branches(name PK, task_id, created_at)`, `sessions(id PK, branch FK, type, status, model, effort, created_at, updated_at)`.
- Typed error hierarchy; errors published as `error.raised` events with severity.
- Structured logging setup.
- CI config (GitHub Actions): fmt, clippy, eslint, tsc, tests.
**DoD:** empty window opens; a test event emitted in core arrives in frontend state; migrations run idempotently; CI green.

### T2 — GitProvider + worktree manager
- `GitProvider` trait: `list_worktrees`, `create_worktree(branch, base)`, `remove_worktree`, `branch_status` (clean/dirty, ahead/behind), `merge_base_diff` (raw for T5). Impl over git CLI with proper error mapping.
- Worktree manager: create worktree with branch naming convention `{type}/{task-id}-{slug}` (convention configurable), pick base branch, persist branch row, emit `worktree.created`. Remove with dirty-check confirmation; branch state in store survives.
- UI: WorktreeList view showing worktrees with git status; create/remove dialogs.
**DoD:** create/remove/switch worktrees from UI; re-creating a worktree for an existing branch reattaches its stored state.

### T3 — Sidecar + AgentEngine + Session lifecycle (architecturally the most important task — take extra care here)
- Define the Rust↔sidecar protocol first (versioned, NDJSON over stdio):
  - requests: `spawn {session_id, cwd, prompt, model, effort, session_type, permission_mode, resume_id?}`, `send {session_id, prompt}`, `interrupt {session_id}`, `shutdown`
  - events: `stream_delta {session_id, text}`, `tool_use {session_id, name, summary}`, `permission_request {session_id, request_id, tool, args}`, `status {session_id, status}`, `result {session_id, ...}`, `error {session_id?, ...}`
  - responses to permission: `permission_response {request_id, allow, updated_args?}`
- Sidecar: wraps Claude Agent SDK `query()`; maps SDK stream to protocol events; supports resume and canUseTool→permission_request bridging.
- Rust `agent/`: sidecar supervisor (launch, restart on crash, request/response correlation) behind `AgentEngine` trait.
- Rust `session/`: state machine, spawn-time binding to branch, persistence of session rows and SDK session ids, cancellation, reconnect (after sidecar restart, sessions are re-listed from store and marked `failed` if unrecoverable).
- Model/effort passed per spawn; changeable for new sessions from UI.
**DoD:** two parallel chat sessions in one worktree stream simultaneously; cancel works; killing the sidecar process recovers gracefully; statuses propagate to WorktreeList via events.

### T4 — Chat panel
- Multiple chats per worktree (tabs), default type `manual`.
- Single-writer rule enforced in the session module (not UI): at most one session with write permissions per worktree at a time; others spawn in read-only permission mode or queue (configurable). 
- Rendering: markdown, code blocks, collapsible tool-use entries, permission requests inline with allow/deny buttons (wired to protocol).
- Model/effort selector per new session.
**DoD:** fully usable Claude Code chat in worktree context.

### T5 — Diff engine + viewer (strictly scoped — do not gold-plate)
- core/diff: unified diff of branch vs merge-base with its base, changed-file list, on-demand blame for a line range. Cache per branch, invalidate on `session.status_changed(done)` and manual refresh; emit `diff.updated`.
- UI: file list sidebar + CodeMirror 6 unified diff view (use @codemirror/merge), standard syntax highlighting. Fast switching between worktrees' diffs.
- **Out of scope (do not implement):** side-by-side view, word-level highlighting, context folding, image diffs, virtualization (only add if a real diff is measurably slow).
**DoD:** switching diffs between worktrees < 1s on a medium diff.

### T6 — Line-level questions in diffs
- Select lines → build context (file path, hunk text, blame, branch) → render prompt from the `line-question` template → send to the worktree's most recent active session via resume, or a fresh short-lived session (config option).
- Answers render as inline blocks between diff lines (simple block insertion — no floating anchored bubbles).
- If the user navigated away before the answer arrived → `attention.required`.
**DoD:** select → ask → answer appears inline; works on several worktrees in parallel.

### T7 — Gate: registry + commit/PR approval flow
- gate registry: matchers on (tool, args pattern) → handler. Register: `git push`, `gh pr create`, and optionally `git commit` (config).
- Implementation via Agent SDK permission/hook mechanism surfaced through the sidecar protocol: matching tool call pauses → `gate.pending` event with extracted params (commit message / PR title+body) → GateDialog with editable fields → allow (with edited params substituted into the tool args) / deny (with optional feedback text returned to the agent).
- Nothing matched by the registry ever executes without explicit approval.
**DoD:** agent prepares commit and PR; user edits message/title in dialog; only approved content is pushed.

### T8 — Prompt templates
- `~/.maestro/prompts/*.md`, frontmatter (`name`, `description`, `variables`), `{{var}}` rendering, defaults copied on first run, per-worktree CLAUDE.md respected by the SDK as-is.
- Default templates: `commit-message`, `pr-description`, `line-question`, `task-notes` (with Decisions / Trade-offs / Open questions sections — groundwork for Stage 2).
- UI: PromptEditor (textarea per template + reset-to-default).
**DoD:** editing a template changes real prompts without restart.

### T9 — Attention panel + statuses
- Aggregate `attention.required` sources (agent question, permission request, gate.pending, session failed) into one queue; click navigates to the source (chat / gate dialog / diff).
- WorktreeList badges: working / awaiting_input / diff_ready / failed.
- OS notifications via Tauri notification plugin (config-gated).
**DoD:** with 3–4 agents running, one glance shows who needs the user.

### T10 — Polish & dogfood buffer
- Reconnect edge cases, empty states, app config file (`~/.maestro/config.toml`), hotkeys for worktree switching, error toasts wired to `error.raised`.
- Fix whatever real usage surfaces.

## Dependency graph

```
T1 → T2 → T5 → T6
 └─→ T3 → T4
      ├─→ T7 (needs SDK bridge)
      └─→ T9 (bus exists from T1; fed by T3/T5/T7)
T8: independent after T1
```

## Stage Definition of Done

I can: create 3 worktrees for 3 tasks, run an agent in each, see in one panel who is working and who awaits me, review diffs asking questions on specific lines, approve and push commits/PRs without leaving the app — all driven by prompt templates I've edited.

## Explicitly deferred (do NOT build now)

Background daemon / GitHub polling; cross-agent context escalation (`ask_original_agent`); rebase-all; side-by-side diff; usage gating. The architecture above (event bus, session types, sidecar, branch-keyed store) must leave clean seams for them — that is enough.
