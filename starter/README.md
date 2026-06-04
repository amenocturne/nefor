# nefor-team starter config

Team configuration for the nefor engine — DP→JWT auth against Nestor,
the lead orchestrator workflow, Jira/Confluence tools, and a think-tag
filter for qwen-family models. This is a plain consumer of upstream
nefor: generic engine plumbing comes from upstream via `nefor-pm`; this
directory carries only team-specific overrides.

For run / sync instructions see [`../README.md`](../README.md) and the
repo-level [`justfile`](../justfile).

## Exact version pin

The repo root `.env` is the source of truth for the runtime version:

```sh
NEFOR_VERSION=0.3.0
```

`just sync` reads that file, asks for explicit confirmation, installs the exact
pinned nefor version, overwrites `~/.config/nefor` from `starter/`, and copies
`.env` to `~/.config/nefor/.env`. It does not run tests and must not mutate repo
files.

At startup `init.lua` reads `NEFOR_CONFIG_DIR/.env` first, then falls back to
`NEFOR_CONFIG_DIR/../.env` when the local file is missing or empty. It
hard-errors when `nefor.version` differs from `NEFOR_VERSION`, with a `just
sync` remediation. That keeps the installed binary and installed Lua config in
lockstep while allowing `just run` to use the repo-root pin.

## File inventory

### Composition root

- `init.lua` — checks the exact version pin, bootstraps nefor-pm,
  installs plugin libs, grafts package paths, and composes the team's
  variant-driven actor graph (Nestor / ollama / mock).

### Team-only modules

- `config/init.lua` — variant table (prod=Nestor / test=ollama / mock),
  per-role model pinning via `workflow.role_models`, and binary-path
  resolver (`config.bin("<name>")`).
- `lead-workflow/role.lua` — role roster: `explorer`, `worker`,
  `reviewer`, `docs`, `critic`. Exposes `LEAD_SYSTEM_PROMPT`,
  `AGENT_CONFIGS`, `ORCHESTRATION_TOOLS`, and `TOOL_ALLOWLIST`.
- `auth/init.lua` — DP CLI subprocess + JWT exchange against Nestor's
  `/api/v2/token`. Used only by the Nestor variant.
- `compositors/qwen_hooks.lua` — team-owned hooks wired into upstream's
  provider compositor for qwen think-tag filtering and Nestor model-list
  interception.

### Team-only prompts

- `prompts/lead.md` — Qwen-oriented lead orchestrator prompt with explicit
  routing rules, planning/critic workflow, approval semantics, and graph rules.
- `prompts/explorer.md` — read-only investigation.
- `prompts/worker.md` — general write-capable approved-work executor.
- `prompts/reviewer.md` — read-only review.
- `prompts/docs.md` — Jira/Confluence/local docs agent; write-capable for
  approved documentation changes.
- `prompts/critic.md` — read-only plan critique.

Tests live outside `starter/` under [`../tests/lua/`](../tests/lua/) so the
installed config has no test code.

## Approval policy

Plan approval gates only write-capable roles: `worker` and `docs`.
Read-only roles (`explorer`, `reviewer`, `critic`) may dispatch without plan
approval. After approval, normal file edit/write tools inside write-capable
agents do not require another plan approval.

Permission modes mirror upstream: `/safe` is the default interactive
approve/deny/defer mode; `dispatch-graph` and ambiguous `bash` stay
runtime-gated there. `/auto` is unattended approve/deny-only mode: anything
that would defer to the user is denied with recovery text. `/yolo` fully
approves all tool calls.

The team config declares read-only/write-capable role metadata explicitly via
`AGENT_CONFIGS[role].read_only`; it does not rely on an implicit upstream helper
or missing default.

## Run modes (NEFOR_DEV_DIR)

The team's `init.lua` supports two run modes:

- **Dev mode** (`NEFOR_DEV_DIR=/path/to/personal/nefor`) — `pm.install` specs
  use local `dir` overrides and `STARTER_UPSTREAM = NEFOR_DEV_DIR/starter`.
- **Prod mode** (`NEFOR_DEV_DIR` unset) — the upstream nefor repo is sparse
  cloned under `$NEFOR_DATA_DIR/nefor/` and pinned from the running engine
  version (`0.3.0` → `v0.3.0`; dev/nightly versions fall back to `main`).

## Sync procedure when upstream advances

1. Update root `.env` to the new target `NEFOR_VERSION`.
2. Inspect upstream changes for actor/spec shape, lead-workflow API,
   agentic-loop behavior, provider compositor hooks, tool allowlists, and plugin
   binary layout.
3. Apply only team-side overlay changes needed for compatibility; do not rewrite
   toward generated agentic-kit config.
4. Run Lua tests locally. `just sync` intentionally does not run tests.
