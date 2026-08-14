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
  The editor previews any draft with sample variables; templates you haven't touched
  pick up improved defaults on update, and ones you've edited are never overwritten.
- **Cross-agent context that survives a session ending.** An implementation session
  writes `TASK_NOTES.md` on close; a review session reads it and can escalate a direct
  question back to the implementer instead of guessing.
- **Blast radius.** Before trusting a change, see what it implicitly touches: the app
  scans the repo for modules that import or reference what the diff changed and lists
  the likely-affected dependents — with a one-click "verify these with the main agent."
- **A red-team antagonist (QA).** One click branches a child worktree off a task and
  spawns an agent whose only goal is to break the code — edge cases, race conditions,
  failing proof-tests. It gets the blast radius as ammunition, can interrogate the
  implementer directly, and writes `REDTEAM.md`; its findings go back to the parent
  branch's agent for rework, then the whole child worktree is dismantled. An opt-in
  `red_team_auto` mode attacks every finished implementation session without being
  asked (and announces itself when it does).
- **An interactive review guide.** For a human reviewing a big diff: an agent orders
  the changes into a step-by-step roadmap — critical business logic first, boilerplate
  last — and checking off a step marks its files as viewed in the diff viewer.
- **A fleet view.** A header chip shows how many agents are streaming or waiting on
  you right now, with per-session cost; one attention queue (with a rebindable hotkey
  and a dismiss-all) collects everything that blocks anyone — permission prompts,
  gates, questions, failures, red-team findings. Worktree rows carry the same signals
  at a glance: pending-attention badges, lifetime agent cost, and a warning when a
  session is close to filling its context window.

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

All persistent state (SQLite store, prompt templates, opt-in local conversation logs
with configurable retention) lives under `~/.maestro` (or `$MAESTRO_HOME`), never
inside the repo it's orchestrating.

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

## More docs

- [ARCHITECTURE.md](ARCHITECTURE.md) — the module map and the invariants the
  design leans on.
- [CHANGELOG.md](CHANGELOG.md) — what shipped, grouped by era.
- [docs/perf-baseline.md](docs/perf-baseline.md) — reference startup/suite
  numbers for spotting regressions.

## License

[MIT](LICENSE)
