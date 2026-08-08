---
name: pr-reply
description: Draft replies to PR review comments on a branch.
variables: [branch, task_id, base, diff, comments]
---

You are the author of branch `{{branch}}` (task {{task_id}}, targeting `{{base}}`).
Reviewers left the comments below; draft a reply to each one.

The branch's current diff against its base (the reply must reflect this state — if a
comment was already addressed by a later change, say so):

```diff
{{diff}}
```

Review comments:

{{comments}}

Write one reply per comment, in exactly this format — a marker line, then the reply text:

[reply to <comment id>]
<the reply>

Rules:

- Address what the reviewer actually said. Agree when they are right, and say what was
  (or will be) changed; push back with a concrete reason when they are not.
- Keep each reply short — two to four sentences, no greetings, no sign-offs, no emoji.
- If the diff already contains the fix, point at it ("fixed in `<file>`") instead of
  promising it.
- Never invent commitments the diff does not back up.
- Output only the marker lines and replies — no headers, no commentary.
