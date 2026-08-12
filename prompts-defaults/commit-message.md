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

Rules — match this author's actual style, observed from their own commit history:

- One line only. No body, no blank line, no "why" paragraph — say what the commit does and
  stop. A body is the rare exception, not the default.
- Lowercase start (unless the first word is a proper noun/identifier like `LinkedIn` or
  `SQL`), imperative mood, no trailing period, under ~90 characters.
- Prefix with the task id when there is one, using a dash: `{{task_id}} - <summary>`. Skip
  the prefix on small follow-up commits within the same piece of work (fixes, test
  updates, cleanups) — only the commit that introduces the actual change gets the id.
- Name the concrete thing that changed (the method/class/field/table, or the exact
  behavior), not a vague category. "fix test", "remove redundant check", "extract X
  helper" are typical; "improve code" is not.
- Several small changes in one commit can be comma-separated on the same line
  ("dedupe test, drop FT try/catch, use shared entity ids") rather than split into bullets.
- Output only the commit message — no code fences, no commentary.
