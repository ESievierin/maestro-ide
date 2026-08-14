# Architecture

Three processes, one owner of truth:

```
React frontend (src/)          renders state, sends commands — no business logic
        │  Tauri IPC + one event channel
Rust core (src-tauri/)         owns ALL state: SQLite, git, sessions, gates
        │  NDJSON over stdio
Node sidecar (sidecar/)        embeds the Claude Agent SDK; executes sessions
```

## Invariants the whole design leans on

- **The branch is the primary key.** A task lives in its own git worktree; one
  branch ⇔ one worktree ⇔ one persistent main agent. Everything (diffs, notes,
  sessions, cost, attention) is keyed by branch.
- **Events, not calls.** Every state change is published on one in-process bus
  (`core::bus`). Consumers (attention queue, diff invalidation, auto-red-team,
  the frontend forwarder) subscribe; nothing polls another module's internals.
- **Single writer per worktree.** A second write-capable session on a branch is
  downgraded to read-only or rejected (configurable) — two agents never edit
  one checkout concurrently.
- **Outward writes are human-gated.** `git commit`/`push`/`gh pr create` pause
  in a PreToolUse hook until a person approves — even in auto permission mode.
- **The frontend is a face.** If the UI dies, the core's state is still
  correct; panels refetch on events instead of mirroring them incrementally.

## Core modules (`src-tauri/src/core/`)

| Module       | Responsibility                                                                                                                                                                                              |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bus`        | The broadcast event bus; every other module publishes/subscribes here.                                                                                                                                      |
| `store`      | SQLite behind a `Store` trait: branches, sessions, transcripts, usage, settings, presets, daemon tasks. `VACUUM` support for reclaiming swept rows.                                                         |
| `worktree`   | Git worktree lifecycle: create/remove/list, merge with park-and-restore, snapshots, sync, `ensure_named` for system worktrees (red team).                                                                   |
| `session`    | The session state machine (`spawning → streaming → awaiting_input → done/failed/cancelled`), single-writer enforcement, the persistent main agent (revive + resume), notes-finalize turns, telemetry hooks. |
| `agent`      | The engine boundary (`AgentEngine` trait) and the sidecar supervisor: NDJSON protocol, lazy restart after a crash.                                                                                          |
| `diff`       | Branch/worktree diff snapshots with caching and event-driven invalidation.                                                                                                                                  |
| `gate`       | The registry of gated commands and the approval flow the PreToolUse hook drives.                                                                                                                            |
| `prompts`    | Templates as data in `~/.maestro/prompts`: frontmatter, `{{var}}` rendering, default-install hashing so shipped updates reach unmodified files, draft preview.                                              |
| `notes`      | `TASK_NOTES.md` (and `REDTEAM.md` on red-team branches): the cross-agent hand-off channel.                                                                                                                  |
| `escalation` | `ask_original_agent`: resumes the branch's implementer to answer a live question, with a deadline; red-team askers are redirected to the parent branch.                                                     |
| `attention`  | The "who needs me" queue, derived purely from bus events; priorities, dismissal, OS-notification gating.                                                                                                    |
| `questions`  | Select-lines-and-ask on the diff: line questions routed to the active session or a fresh read-only one.                                                                                                     |
| `checks`     | The configurable check command (build/tests) per worktree, with an auto-run-on-finish mode.                                                                                                                 |
| `daemon`     | GitHub/Jira watcher: assigned issues, PR review comments → prepared sessions; never writes to GitHub itself.                                                                                                |
| `impact`     | Blast radius: bounded text scan for files importing/referencing what a diff changed (two rings).                                                                                                            |
| `redteam`    | The QA antagonist: launch (shared by the IPC command and the `red_team_auto` bus loop), child-worktree naming, launch announcements.                                                                        |
| `compose`    | Branch context (commits, file stat, truncated diff) rendered into whole-branch prompts (review guide, commit/PR text).                                                                                      |
| `pr`         | PR creation and review-comment reading/replying through `gh`, token passed per call.                                                                                                                        |
| `telemetry`  | Append-only JSONL conversation logs under `~/.maestro/telemetry`, with a configurable retention sweep at startup.                                                                                           |
| `health`     | Read-only setup diagnostics: git, node, sidecar script, gh, editor, Jira, repository, config.toml parse, telemetry/store footprints.                                                                        |
| `backup`     | Export/import of the portable settings subset (allowlisted) plus owned prompt templates.                                                                                                                    |
| `config`     | `~/.maestro/config.toml` seeds the settings table at startup; one runtime lookup path (`Store::get_setting`).                                                                                               |
| `launcher`   | Opening worktrees/files in the external editor or file explorer.                                                                                                                                            |

## Frontend (`src/`)

Zustand stores mirror core state per domain (`state/`), hydrated by fetch and
kept fresh by the single `maestro:event` channel (`state/events.ts` fans out).
Views (`views/`) are panels over those stores; `utils/actions.ts` holds the
user-facing verbs (start red team, dismantle, export, sweeps). Navigation that
means a specific conversation sets `focusRequest` in the sessions store so the
session panel lands on the right tab.

## Sidecar (`sidecar/`)

`main.ts` speaks the NDJSON protocol and embeds the Claude Agent SDK; `mock.ts`
(`MAESTRO_SIDECAR_MOCK=1`) is a scriptable stand-in whose word-boundary
triggers (GATE, ASK, PLAN, ESCALATE, AUTH, REVIEW_COMMENTS, CRASH, roadmap…)
drive every dialog and failure path in live UI tests without spending tokens.

## State on disk

Everything persistent lives under `~/.maestro` (or `$MAESTRO_HOME`): the
SQLite store, prompt templates, telemetry logs, config.toml. Nothing is written
into the repositories being orchestrated except the worktrees themselves and
the `TASK_NOTES.md`/`REDTEAM.md` files that belong to their branches.
