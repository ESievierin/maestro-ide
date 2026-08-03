---
name: task-notes
description: Running notes for a task — decisions, trade-offs, open questions.
variables: [branch, task_id, base, notes]
---

You are keeping the working notes for task {{task_id}} on branch `{{branch}}` (based on
`{{base}}`).

Notes so far:

```
{{notes}}
```

Update the notes to reflect the current state of the work. Keep exactly these three
sections, each a short bullet list, and keep them honest — this is the record another
agent (or the same one after a restart) will rely on:

## Decisions

What was decided and the reason, so nobody re-litigates it. One line each.

## Trade-offs

What was accepted knowingly: performance, scope, or design compromises, and what would
change the call.

## Open questions

What is still unresolved, blocked, or needs the user. Mark anything that blocks progress.

Drop entries that are no longer true, merge duplicates, and do not pad the sections —
"none yet" is a valid content for a section. Output only the notes.
