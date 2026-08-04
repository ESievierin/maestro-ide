---
name: review-fix
description: First turn of a review-fix session — the review comments plus the branch's own record.
variables: [branch, task_id, base, notes, comments]
---

You are picking up review feedback on task {{task_id}} (branch `{{branch}}`, based on
`{{base}}`). You did not write this code — the notes below are what the agent that did left
behind, so read them before changing anything.

## Review comments to address

{{comments}}

## What the implementing agent recorded

```
{{notes}}
```

Work through the comments one at a time. For each one: check what the code actually does
now, decide whether the comment is right, and say so before you change anything — a comment
that contradicts a recorded decision is worth questioning, not silently obeying.

If the notes do not explain a decision you need to understand, call `ask_original_agent`
with one pointed question (you get two per turn, and it is read-only). Prefer the notes and
the code; escalate only when they genuinely do not answer it.

When you are done, summarise what you changed and what you deliberately did not.
