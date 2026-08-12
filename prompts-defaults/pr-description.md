---
name: pr-description
description: Write a pull request title and description for a branch.
variables: [branch, task_id, base, commits, files, diff]
---

Write a pull request for branch `{{branch}}` (task {{task_id}}) targeting `{{base}}`.

Commits:

```
{{commits}}
```

Changed files:

```
{{files}}
```

Diff:

```diff
{{diff}}
```

Match this author's actual style, observed from their own PR history:

TITLE: one line, imperative, under 72 characters. Prefix with the task id and a dash when
there is one (`{{task_id}} - <summary>`); otherwise just the summary.

BODY — scale it to the change, don't pad:

- A small, self-explanatory change (a one-line fix, a rename, a toggle removal) gets no
  body at all. Leave it empty rather than restating the title in prose.
- A bug fix gets a short root-cause note: what broke, why (name the actual code path —
  method/class/line — not a vague description), then the fix, in one or two sentences. No
  headers needed for something this size.
- A feature or anything with real design decisions gets `## What` (and `## Why` when the
  motivation isn't obvious from the ticket/title) as short bullets, not paragraphs —
  grouped by area, using the actual names of the classes/endpoints/tables/feature-toggles
  involved. Add `## How` only when the approach itself needs explaining, not just the
  outcome.
- Always call out feature-toggle gating explicitly when there is one: the toggle name, and
  what happens with it off — that's the rollback story, worth stating even when it's "no
  change".
- Mention tests only as a short trailing clause when it adds information ("covered by unit
  - integration tests", "manually verified against X") — never a formal checklist.

No filler, no "as requested", no restating the diff line by line, no invented sections
that don't apply to this change.
