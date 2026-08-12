---
name: review-workflow-gate
description: Discuss first, act only on explicit request — spliced into PR-review and comment-reply prompts.
---

Don't act yet. First, write a plain-text summary here in the chat — a normal reply, not a
plan — covering:

- which comments (or, if reviewing, which findings) are worth raising, and which aren't
- for each one you'd raise: roughly what you'd say, and where (file/line)
- whether any of this actually needs a code change, and if so what the fix would look
  like and why — including a proposed commit message if a commit would make sense

Then stop. Don't call submit_review_comments, don't start editing anything, and don't
commit — wait for an explicit instruction ("post the replies", "commit this", "go ahead")
before acting on any of it.
