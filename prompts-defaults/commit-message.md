---
name: commit-message
description: Write a commit message for the changes on a branch.
variables: [branch, task_id, base, files, diff]
---

Write a commit message for the changes on branch `{{branch}}` (task {{task_id}}, based on
`{{base}}`).

Changed files:

```
{{files}}
```

Diff:

```diff
{{diff}}
```

Rules:

- First line: imperative summary under 72 characters, no trailing period. Prefix with the
  task id when there is one (`{{task_id}}: …`).
- Then a blank line, then a short body explaining **why** the change was made and any
  decision a reviewer would otherwise have to reverse-engineer. Skip the body only when
  the summary genuinely says everything.
- Describe what the change does, not the process of writing it. No "as requested", no
  file-by-file narration of things the diff already shows.
- Output only the commit message — no code fences, no commentary.
