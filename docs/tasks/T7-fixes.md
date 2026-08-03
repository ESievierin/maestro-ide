# T7 — fix round (review findings)

Continue in the same worktree/branch (`impl/T-7-commit-pr-gate`). The gate is the
approval boundary before an agent can push, open a PR, or commit — it must be
**fail-closed**: if the tool call plausibly runs one of those, it gates; if the params
cannot be extracted or edited unambiguously, the dialog shows the raw command with
Allow/Deny instead of a misleading editable field. Review found several fail-open paths
and a UI wedge. Fix all items below with tests, commit. Never push.

Read `docs/tasks/T7-commit-pr-gate.md` again for the original spec and rules.
The quoting/escaping, span-splicing, permission-flow shape, manager hook, and
`gate_commit` handling were reviewed as correct — do not restructure them.

## 1. CRITICAL — Line continuations defeat every matcher

`core/gate/rules.rs:140` — the `'\\'` branch pushes the next char into the token, so a
backslash-newline becomes a literal newline **inside** the token: `git push \`⏎`  --force`
tokenizes as `["git", "push\n--force"]` and `git_subcommand` no longer sees `push`. This
is the ordinary shape agents emit for long commands, so the gate silently fails open in
normal use.

Fix: treat backslash-newline as a line continuation (consume both characters, emit
nothing, do not break the token). Tests: `git push \`⏎`origin main`, `git \`⏎`push`,
`gh pr create \`⏎`--title "x" --body "y"`.

## 2. CRITICAL — Wrapper / grouping / substitution bypasses

Matching requires the segment's first non-`VAR=` token to be literally `git`/`gh`.
Verified non-matching today: `sh -c "git push"`, `bash -c '…'`, `eval "git push"`,
`command git push`, `env git push`, `time git push`, `nohup git push &`,
`/usr/bin/git push`, `(git push)`, `{ git push; }`, `$(git push)`, backticks,
`if true; then git push; fi`, `for … do git push; done`, `xargs -I{} git push`.

Fix (fail-closed, in this order):
- Compare the **basename** of the program token (`/usr/bin/git` → `git`).
- Strip leading wrapper words before deciding the program: `command`, `env`, `time`,
  `nohup`, `exec`, `sudo`, `stdbuf`, plus `VAR=value` assignments (already handled).
- Unwrap grouping tokens (`(`, `)`, `{`, `}`) and shell keywords that can precede a
  command (`then`, `else`, `do`, `fi`, `done`, `elif`) as separators, so the command
  after them is matched.
- Recurse into nested command strings: for `sh`/`bash`/`zsh`/`dash` with `-c <string>`
  and for `eval <string>`, re-tokenize the string argument and match inside it; also
  recurse into `$( … )` and backtick bodies.
- When recursion finds a match, the gate has **no editable params** (params empty, raw
  command shown) — splicing into a nested quoted string is not safe. Prefer approving
  the raw command over pretending it is editable.
- Tests for every bypass listed above (assert they now match), plus a control set of
  commands that must NOT match (`git status`, `gh pr view`, `echo "git push"` — note the
  echo case is expected to match under a conservative matcher; if you decide it matches,
  assert that explicitly and document the false-positive as intentional).

## 3. HIGH — Only the first matching rule fires, so PR params are lost

`core/gate/mod.rs:74` — `match_tool` returns the first match, and `git_push` is
registered first. The canonical agent command
`git push -u origin HEAD && gh pr create --title "…" --body "…"` therefore gates as
"git push" with **no editable params** — the user cannot edit the PR title/body, which is
the task's DoD.

Fix: evaluate all rules and choose deterministically by priority
`gh_pr_create > git_commit > git_push` (most editable/most consequential first), or merge
the matched params from several rules into one gate (a combined command genuinely does
both). Whichever you choose, the dialog must let the user edit the PR title/body for the
combined command, and `apply` must splice each param back into its own segment. Add a
registry-level test with exactly that combined command (the existing
`pr_create_apply_only_touches_the_pr_segment` test bypasses the registry, which hid this).

## 4. HIGH — Only the first segment / first flag occurrence is extracted

`find_flag` returns on the first hit and the rules use `.find(...)` over segments, so:
- `gh pr create --title A --body B --title EVIL` shows/edits `A`, but `gh` uses the last
  `--title` → the approved title is discarded and `EVIL` executes.
- `git commit -m "a" && git commit -m "b"` shows only `a`; the second commit executes
  with unapproved `b`.

Fix (fail-closed): detect multiplicity — more than one matching segment for a rule, or
more than one occurrence of a param flag within a segment — and in that case emit the
gate with **no editable params** plus a `note` field explaining why ("two commits in one
command — approve the command as-is or deny"). Add the `note: Option<String>` to the gate
payload and render it in the dialog. Tests for both scenarios.

## 5. MEDIUM — File-based / auto-filled message sources look empty and corrupt the rebuild

`git commit -F msg.txt`, `--file=…`, `gh pr create --body-file b.md`, `gh pr create
--fill` match but yield an empty param; typing into that field appends a second
mutually exclusive flag and git/gh reject the command.

Fix: detect these sources and emit the gate with no editable params + a `note` naming the
source (e.g. "message comes from msg.txt"). Optionally read small files to show the
content read-only — not required. Tests for each flag form.

## 6. HIGH — Pending gates leak and wedge the whole UI

Nothing removes entries from `GateManager::pending` when a session closes, crashes, or is
swept by `fail_stale_sessions`. `GateDialog` is a fixed full-screen backdrop with no
dismissal, so a dead gate blocks the entire app; clicking Allow "succeeds" silently
(the sidecar acks `false` for the unknown request id) and the user believes the push was
approved.

Fix:
- `GateManager::cancel_for_session(session_id)` removing all its pending gates; call it
  from `SessionManager` on `SessionClosed`, in `handle_crash`, and in
  `fail_stale_sessions`.
- Publish a new bus event `gate.resolved { gate_id, reason }` (append to
  `core/bus/mod.rs`) whenever a gate leaves the pending set — answered by the user,
  cancelled, or session-dead — and remove it from the frontend store on that event.
- `respond` must return a typed error the frontend can distinguish for an unknown
  gate_id, and `src/state/gates.ts` must drop the entry when that error comes back
  (today it keeps it, so the modal can only be cleared by reloading the app).
- Tests: pending gate + session closed → gate gone + `gate.resolved` published; respond
  to an unknown gate → typed error.

## 7. LOW — `find_flag` aborts the search on a valueless flag

`rules.rs:417` — `segment.get(i + 1)?` returns from the function instead of continuing,
so `git commit -m` (dangling) hides a later `--message=x`, and the rebuild produces
`git commit -m --message 'approved'`. Fix: continue scanning.

## 8. LOW — Top-level here-docs are not tokenized

`consume_heredoc` is only reachable from inside `consume_substitution`, so
`git commit -F- <<'EOF' … EOF` splits the body into fake command segments (a body line
reading `git push origin main` raises a spurious gate). Fix: handle `<<` at the top level
too.

## 9. Guard the boundary: drop `bypassPermissions` from the UI

`src/types/sessions.ts:52` offers `bypassPermissions`; with it the SDK never calls
`canUseTool`, so no permission request is emitted and the gate never sees the push —
"nothing matched by the registry ever executes without explicit approval" no longer
holds. Remove that option from the selector list (keep the plumbing so a config file can
still set it) and note it in the file with a short comment.

## Checks

```
cd src-tauri && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && npm run lint && npm run typecheck && npx prettier --check src && npx vite build
```

Do not touch `sidecar/`, `core/diff/*`, `core/prompts/*`, `core/questions/*`,
`src/views/DiffViewer.tsx`, `src/state/questions.ts` (a parallel branch owns those).
