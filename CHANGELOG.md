# Changelog

A thematic history of what shipped, newest era first. MaestroIDE moves in
continuous improvement cycles rather than versioned releases; entries here are
grouped by what they add up to, not by date.

## Fleet automation era

The fleet stopped being a set of tabs and became something the app manages
with you.

- **Auto-red-team** (`red_team_auto`, off by default): every implementation
  session that finishes with committed changes gets the QA antagonist launched
  against it automatically — and the launch announces itself in the attention
  queue.
- **Red-team lifecycle, end to end**: findings land as an attention item that
  opens REDTEAM.md directly, the Notes tab carries "send to parent" and
  "dismantle" actions, removing a parent worktree offers to take its red team
  down too, and the red team can interrogate the original implementer.
- **Fleet legibility**: a header chip lists every working/awaiting agent with
  its spend; worktree rows carry pending-attention badges, lifetime cost
  (persisted across restarts), and a warning when a session nears its context
  window; the attention drawer has a rebindable hotkey (Alt+A) and dismiss-all.
- **Housekeeping that scales**: clear finished sessions across every branch in
  one palette action, with an automatic SQLite VACUUM after big sweeps; a
  manual "Compact store" button; configurable telemetry retention.
- **Setup diagnostics**: the health check now covers git, node, the sidecar
  script, gh, the editor, Jira, the repository, config.toml parsing, and the
  disk footprint of both the telemetry logs and the store.
- **Settings grew a face for everything**: red-team model/effort and auto
  mode, telemetry retention, escalation timeout, notes-finalize timeout,
  branch naming, an About block (version, state directory, engine mode).
- **Quality of life**: role-named session tabs, a visible "writing
  TASK_NOTES.md" marker while a session closes, session-search role filter,
  event-log filter, prompt-template preview with sample variables, bulk
  viewed/unviewed in the diff, a mock-engine indicator.

## Review intelligence era

Three features that turn a diff into something reviewable, and the connective
tissue between them.

- **Blast radius**: a bounded scan of the repository for files that import or
  reference what a branch changed — two rings of dependents, cached per branch
  with a staleness badge, and a one-click "verify these with the main agent".
- **QA antagonist (red team)**: one click branches a child worktree off a
  task's committed state and spawns an agent whose only goal is to break the
  changes — edge cases, race conditions, failing proof-tests — writing its
  findings to REDTEAM.md for a human-mediated handoff back to the implementer.
- **Interactive review guide**: the branch's main agent lays out the reading
  order for a diff (core logic first, boilerplate last); checking off a step
  marks its files viewed, stale guides say so, and steps whose files have
  outside dependents carry a blast-radius warning chip.
- **A persistent main agent per worktree** that survives app restarts with its
  SDK context intact, revives on demand, and prunes its superseded rows.

## Workflow era

Everything between "agent wrote code" and "PR merged".

- **GitHub daemon**: watches assigned issues and PR review comments (optionally
  Jira), prepares research/review sessions, never writes to GitHub itself;
  multi-account polling, label skips, manual poll, retry on transient failures.
- **PR workflow**: seamless merge, Create PR with agent-generated descriptions,
  structured review-reply drafting through the branch's own main agent, and a
  personal style guide so generated text sounds like its author.
- **Cross-agent context**: TASK_NOTES.md written on close and read by whoever
  picks the branch up next; `ask_original_agent` as a live escalation channel.
- **Guard rails**: commit/push/PR gates as a PreToolUse hook (safe even in
  auto permission mode), single-writer policy, snapshots of uncommitted state.
- **Operator conveniences**: command palette, configurable hotkeys, session
  presets, cross-branch transcript search, transcript export/copy, worktree
  pinning/filtering/bulk sync/bulk push, settings backup/import, usage and
  cost breakdowns, conversation telemetry with retention.

## Foundation

- Tauri 2 desktop app: Rust core (event bus, SQLite store, worktree manager,
  session state machine, diff engine, gate registry, prompt templates), React
  frontend, Node sidecar embedding the Claude Agent SDK over NDJSON stdio.
- Git worktrees as the unit of work: one task, one branch, one worktree, one
  persistent main agent.
- A diff viewer built for review: split/unified, worktree vs committed scope,
  CRLF/LF handling, select-lines-and-ask, viewed checkmarks that persist.
