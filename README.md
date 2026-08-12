# MaestroIDE

A Tauri desktop app for orchestrating parallel agentic development on top of git
worktrees and Claude Code (via the Claude Agent SDK). The central entity is a
task/worktree, not a file: you manage a fleet of agents, review their diffs, gate what
they're allowed to commit or push, and see at a glance which one needs you.

## What it does

- **Worktrees, not branches-in-place.** Each task gets its own git worktree, so multiple
  agents can work on the same repo in parallel without stepping on each other's checkout.
- **Sessions with a state machine, not a black box.** Every agent run is a first-class
  session (`spawning → streaming → awaiting_input → done | failed | cancelled`), bound to
  a branch, resumable, with model/effort/permission-mode switchable mid-run.
- **A persistent main agent per worktree.** One session per worktree that never closes —
  it's what PR review, review-comment replies, and commit/PR-description generation
  resume, so the conversation carries context across turns instead of starting cold every
  time. It discusses findings in plain text first and only acts once you say so.
- **A commit/push/PR gate.** Dangerous operations are matched against a registry of rules
  before an agent can run them — a human approves, every time, no exceptions baked in per
  tool.
- **A background daemon (opt-in).** Watches GitHub (and optionally Jira) for review
  requests, new review comments on your PRs, and assigned issues, and prepares a session
  for each — but never posts or commits anything itself. A human always approves the
  actual GitHub/Jira write through the same gated flow the interactive UI uses.
- **A diff viewer built for review**, not just `git diff` in a terminal: split/unified,
  worktree vs. committed scope, line-ending fixups, snapshot-backed checkpoints of
  uncommitted state you can take and restore.
- **Prompts as data.** Commit messages, PR descriptions, and review-reply drafts are
  generated from editable markdown templates in `~/.maestro/prompts/` — including,
  optionally, a personal style guide so generated text sounds like you, not a generic bot.
- **Cross-agent context that survives a session ending.** An implementation session
  writes `TASK_NOTES.md` on close; a review session reads it and can escalate a direct
  question back to the implementer instead of guessing.

Nothing about GitHub/Jira write access, or what an agent is allowed to run, is silent:
every state change is an event on a central bus, and everything that touches the outside
world goes through a human-approved gate.

## Architecture

- **`src-tauri/`** — Rust core. All business logic lives here: event bus, worktree
  manager, session lifecycle, agent bridge, diff engine, gate registry, prompt templates,
  background daemon, SQLite store.
- **`src/`** — React + TypeScript frontend. Renders state and sends commands; no business
  logic.
- **`sidecar/`** — Node.js sidecar embedding the Claude Agent SDK. Executes agent
  sessions; the Rust core owns all state. NDJSON protocol over stdio.
- **`prompts-defaults/`** — default prompt templates copied to `~/.maestro/prompts/` on
  first run.

All persistent state (SQLite store, prompt templates, opt-in local conversation logs)
lives under `~/.maestro` (or `$MAESTRO_HOME`), never inside the repo it's orchestrating.

## Development

```sh
npm install            # frontend deps
cd sidecar && npm install && cd ..
npm run tauri dev      # launch the app (starts vite + cargo)
```

Rust checks (from `src-tauri/`):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Frontend checks (from repo root):

```sh
npm run lint
npm run typecheck
npm run format:check
npm test
```

## License

[MIT](LICENSE)
