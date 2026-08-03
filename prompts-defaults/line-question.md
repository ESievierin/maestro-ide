---
name: line-question
description: Ask a focused question about a specific range of lines in a diff.
variables: [branch, file, line_start, line_end, hunk, blame, question]
---
You are looking at branch `{{branch}}`, file `{{file}}`, lines {{line_start}}-{{line_end}}.

Selected lines:
```
{{hunk}}
```

Blame for these lines:
{{blame}}

Answer the following question about the selected lines. Be specific and concise, and
reference the exact lines or blame entries where relevant.

Question: {{question}}
