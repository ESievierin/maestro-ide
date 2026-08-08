---
name: pr-reply
description: Follow-up asking an ongoing review session to draft final replies to each comment.
variables: [extra]
---

Based on everything discussed (and implemented, if anything changed) in this conversation,
write the final reply to each review comment listed earlier. Output only this format, one
block per comment, nothing else:

[reply to <comment id>]
<the reply>

Rules:

- Address what the reviewer actually said. Agree when they are right, and say what changed
  — point at the file, not a promise; push back with a concrete reason when they are not.
- Keep each reply short — two to four sentences, no greetings, no sign-offs, no emoji.
- Cover every comment listed earlier, even ones you consider already resolved — say
  "already handled, see `<file>`" for those instead of skipping them.
- Never invent commitments the code does not back up.
- Output only the marker lines and replies — no headers, no commentary.

{{extra}}
