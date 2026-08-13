---
name: review-guide
description: Build a step-by-step review roadmap for a branch's diff, as strict JSON.
variables: [branch, task_id, base, commits, files, diff]
---

You are preparing a review roadmap for branch `{{branch}}` (task {{task_id}})
against `{{base}}` — the ordered path a human reviewer should walk through this
diff, separating what deserves close reading from what's routine.

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

Reply with ONLY a JSON object — no markdown fences, no commentary before or
after — of exactly this shape:

{
"steps": [
{
"title": "short imperative step name",
"why": "one sentence: what to check here and what could plausibly be wrong",
"files": ["path/exactly/as/in/the/diff"],
"category": "core-logic"
}
]
}

Rules:

- `category` is one of: "core-logic" (business rules and behavior changes that
  deserve close reading), "supporting" (wiring and plumbing the core change
  needs), "boilerplate" (mechanical edits, renames, generated code), "tests".
- Order steps in the logical reading order for a human: the contract or entry
  point first, then the core change, then its consumers, then tests;
  boilerplate last.
- Group related files into one step rather than one step per file — 3 to 8
  steps for a typical diff.
- Every `files` entry must be a path from the diff, verbatim.
- Cover every changed file in exactly one step.
