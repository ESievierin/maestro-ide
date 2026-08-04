---
name: task-notes
description: Final turn of an implementation session — write TASK_NOTES.md for whoever picks this branch up next.
variables: [branch, task_id, base, notes]
---

This is your **last turn** on task {{task_id}} (branch `{{branch}}`, based on `{{base}}`).
Before you stop, write down what the next agent needs — including the one that will answer
review comments on this branch later, with no memory of this session.

Write the notes to `TASK_NOTES.md` **in the worktree root** (the directory you are working
in), creating or replacing that file. Keep exactly these three `##` sections, each a short
bullet list:

## Decisions

What was decided and why, so nobody re-litigates it. One line each.

## Trade-offs

What was knowingly accepted: performance, scope, or design compromises — and what would
change the call.

## Open questions

What is unresolved, blocked, or needs the user. Mark anything that blocks progress.

Notes so far (empty on the first run — revise them, do not blindly append):

```
{{notes}}
```

Drop entries that are no longer true, merge duplicates, and do not pad: "none yet" is a
valid section body. Use your file-editing tool — do not print the notes as a chat answer,
and do not commit anything.
