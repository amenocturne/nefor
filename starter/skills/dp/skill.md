---
name: dp
description: Read internal Jira issues via the `dp` CLI. Use for Jira issue
  keys (e.g. ITAL-1234), ticket status, descriptions, and comments. Read-only.
---

# dp

Internal data-platform CLI, already on PATH. Auth is handled by nefor at
startup; if a call reports "not authenticated", ask the user to run
`dp auth login` in a terminal and retry.

## Jira

Fetch a single issue (fields + comments) by key:

    dp jira issue --key ITAL-1234 | jq -Rs 'sub("^[^{]*";"") | fromjson'

`dp` sometimes prepends a human-readable update notice before the JSON.
`jq -Rs` slurps the whole output as one string; `sub("^[^{]*";"")` drops
everything up to the first `{` (a no-op when there is no notice, since it
matches nothing), and `fromjson` parses what's left. So the JSON comes out
clean whether or not the notice is present — no manual checking. `[^{]`
matches newlines too, so a multi-line notice is stripped cleanly.

Chain further filters after it, e.g. just the key fields:

    dp jira issue --key ITAL-1234 \
      | jq -Rs 'sub("^[^{]*";"") | fromjson | {key, summary: .fields.summary, status: .fields.status.name}'

Read-only — there is no write path. Issue links are not available via `dp`.
