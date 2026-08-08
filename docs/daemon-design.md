# GitHub Daemon (Этап 3) — build-ready design

Status: **designed, not yet implemented.** This document turns the Этап 3 idea
list into an architecture mapped onto the existing core, so implementation can
start without re-deciding anything fundamental. It was deliberately _not_ built
unattended: the daemon talks to the real work GitHub account, and its first run
should happen with the user present (auth account choice, thresholds, dry-run).

## What it does

A background loop inside the app (no separate process — the Tauri core is
already long-lived) that watches GitHub and turns two kinds of events into
prepared, human-gated work:

1. **Issue assigned to me** → create a research worktree, run a read-only
   research session (plan mode), write `RESEARCH.md`, notify.
2. **New review comment on a PR whose head branch has a worktree here** →
   verify the comment is actionable (read-only session, visible in the UI like
   any other), prepare a resolution plan and draft replies — then **stop and
   wait for the human** before anything is posted or committed.

## Module layout

```
core/daemon/
  mod.rs        DaemonManager: owns the loop, config, queue
  github.rs     gh CLI boundary (trait GhProvider, like GitProvider) — testable
  events.rs     normalized DaemonEvent { kind, key, payload }
  queue.rs      persistent task queue (SQLite table daemon_tasks)
```

### Polling (`gh api`), not webhooks

- `gh api 'search/issues?q=assignee:@me+is:open'` and
  `gh api 'repos/{owner}/{repo}/pulls/{n}/comments'` per known PR, every
  `daemon_poll_minutes` (default 5). Webhooks/GitHub App + tunnel stay a listed
  future option; polling needs zero infrastructure.
- **Auth**: uses whatever account `gh` is on. The daemon must _check_
  `gh auth status` on start and refuse to run against the wrong account —
  the user has separate personal/work accounts, so the config gets an
  explicit `daemon_expected_login = "..."` guard.

### Routing: branch as primary key

- PR → `head.ref` → worktree with that branch checked out (exact string match,
  same rule the whole app already uses). Task id is parsed out of the branch
  name via the existing `branch_naming` template in reverse.
- Issue → task id (`T-123` / Jira key) → branch name via the template → if a
  worktree already exists for it, the event is a duplicate (see idempotency).

### Idempotency (restart-safe)

- Every event gets a stable key: `issue:{repo}#{number}` /
  `pr-comment:{repo}#{pr}:{comment_id}`.
- `daemon_tasks` table: `key TEXT PRIMARY KEY, state (queued|running|done|dismissed),
created_at, finished_at, session_id, worktree_branch`. Seen keys are skipped
  forever; a crash mid-task leaves `running`, which is re-queued on startup
  (the _session_ it spawned was already failed by `fail_stale_sessions`).
- Worktree creation is itself idempotent: `create` refuses an existing branch;
  the daemon treats that as "already handled".

### Usage gate = a queue, not a binary

- Before starting a task: read the rate-limit info the sidecar already
  reports (`session.rate_limit` events carry utilization of the 5-hour
  window). If utilization > `daemon_usage_threshold` (default 50%), the task
  stays `queued` — nothing is lost, the loop retries each poll tick.
- The queue drains oldest-first, one task at a time (serial by design: the
  background lane must never starve interactive work).

### Model per task type

- `daemon_research_model = "claude-sonnet-5"` (research rarely needs more),
  `daemon_verify_model = "claude-sonnet-5"`; coding stays whatever the user
  picks interactively. These are plain settings — same `config.toml` → settings
  table path as everything else.

### Flows

**Issue assigned**

1. Gate check → create worktree (`research/{task-id}-{slug}` via the template).
2. Spawn a `research` session (read-only plan mode is already a session type +
   permission mode) with the issue title/body/linked context in the prompt,
   instructed to write `RESEARCH.md` in the worktree.
3. On done: OS notification + attention item ("research ready: T-123").

**PR comment**

1. Gate check → find worktree by head branch; skip if none (not our PR).
2. Spawn read-only `review_fix`-style session: verify the comment is
   meaningful/actionable against the actual diff; produce (a) a resolution
   plan, (b) a draft reply per comment — written to the transcript and to
   `REVIEW_PLAN.md` in the worktree.
3. **Mandatory HITL**: nothing is ever posted to GitHub or committed by the
   daemon. The prepared plan lands as an attention item; acting on it is the
   user starting a normal (gated) implementation session, and replies are
   posted only through an explicit approval dialog (same GateManager pattern —
   add a `gh_pr_comment` gate rule so even a user-initiated "post replies"
   pauses on the exact text).

### UI

- A small daemon status chip in the header (off/idle/polling/queued-N), a
  panel listing queue + history (reuses the attention-panel visual language).
- Per-worktree usage counter already exists (this session);
  the daemon adds its own sessions to the same accounting for free.

### Sidecar reuse

- Daemon sessions are ordinary `SessionManager::spawn` calls — same sidecar,
  same events, same transcripts, visible in the UI under their worktree. "The
  daemon reuses the sidecar without UI" is therefore automatic; there is no
  second engine to build.

## Settings (config.toml additions)

```toml
# daemon_enabled = false
# daemon_poll_minutes = 5
# daemon_expected_login = "work-account"
# daemon_usage_threshold = 50        # percent of the 5h window
# daemon_research_model = "claude-sonnet-5"
```

## Implementation order (estimated one focused day each)

1. `GhProvider` + poller + `daemon_tasks` queue + status chip (no actions yet,
   just visible detection — safe to run immediately).
2. Issue → research-worktree flow end-to-end behind `daemon_enabled`.
3. PR-comment → verification/plan flow + `gh_pr_comment` gate rule.

## Open questions for the user (blocking start)

- Which `gh` login should the daemon require (`daemon_expected_login`)?
- Which repos to watch — everything visible, or an allowlist?
- Default usage threshold: 50%? And should interactive work always preempt a
  running daemon session (interrupt), or only block new ones?
