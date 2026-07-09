---
name: confluence
description: Fetch a Confluence wiki page (and its subpages) as Markdown by
  numeric page ID. Use when the user references a Confluence page or a wiki
  URL containing a pageId. Read-only.
---

# confluence

Fetch a wiki page as Markdown via the `confluence` CLI (a self-contained
`uv` script installed on PATH by `just sync`):

    confluence 12345678

Prints the full page as Markdown. If the page has subpages, their IDs are
listed at the end under `Subpages:` — call `confluence <id>` again for each
one you need.

The page ID is the `pageId=` parameter in a Confluence URL, e.g.
`https://wiki.tcsbank.ru/pages/viewpage.action?pageId=12345678` → `12345678`.

Host defaults to the team wiki (override `CONFLUENCE_HOST`); username comes
from `whoami` (override `CONFLUENCE_USER`). Read-only. First run downloads the
`@acq-tech/confluence` npx package, so it may take a moment.

If a fetch fails, report the error and the page ID so the user can verify
access. There is no separate login step — access rides your `whoami` identity.
