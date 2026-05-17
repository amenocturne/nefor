# nefor-team starter config

Team configuration for the nefor engine — DP→JWT auth against Nestor,
the lead orchestrator workflow, and a think-tag filter for qwen-family
models. This is a plain consumer of upstream nefor: all generic engine
plumbing comes from upstream via `nefor-pm` (the package manager
shipped inside the upstream `amenocturne/nefor` repo). This dir only
carries the team-specific overrides.

For run / install instructions see [`../README.md`](../README.md) and
the repo-level [`justfile`](../justfile).

## File inventory

Every file here either implements team-specific behaviour or wires
upstream's plugin-lib primitives into a custom composition. Anything
not in this list lives upstream and is fetched via `nefor-pm`.

### Composition root

- `init.lua` — bootstrap nefor-pm, `pm.install` every plugin lib,
  graft package.path so upstream's starter modules resolve from the
  cloned-or-checked-out upstream tree, then compose the team's
  variant-driven actor graph (Nestor / ollama / mock).

### Team-only modules

- `config/init.lua` — variant table (prod=Nestor / test=ollama / mock).
  Per-role model pinning via `workflow.role_models` and a binary-path
  resolver (`config.bin("<name>")`).
- `lead-workflow/role.lua` — full role roster (lead + 7 sub-agents:
  explorer, builder, reviewer, tester, critic, reflector,
  prompt-engineer). Reads `prompts/<role>.md` off disk; exposes
  `LEAD_SYSTEM_PROMPT` / `AGENT_CONFIGS` / `ORCHESTRATION_TOOLS` /
  `TOOL_ALLOWLIST`.
- `auth/init.lua` — DP CLI subprocess + JWT exchange against Nestor's
  `/api/v2/token`. Used only by the Nestor variant.
- `compositors/qwen_hooks.lua` — team-owned hooks wired into upstream's
  `compositors/provider.lua` via its `opts.hooks` API. Provides:
    - the qwen `<think>...</think>` filter (via
      `openai-provider.think_tag_filter`) on stream.delta,
    - a final-text re-strip at stream.end / chat.complete.result,
    - native-reasoning detection that drops redundant THINKING records
      when the provider already emits structured reasoning_delta,
    - and a drop of outbound `chat.model.list_requested` when the
      Nestor variant's `intercept_model_list_request = true` opt is
      set (Nestor has no `/v1/models`; the cached boot-fetch list is
      served by an `on_event` subscriber wired up in `init.lua`).

### Team-only assets (read-only at runtime)

- `prompts/lead.md` — team-adapted lead orchestrator prompt.
- `prompts/{explorer,builder,reviewer}.md` — team-port (replaces
  upstream).
- `prompts/{tester,critic,reflector,prompt-engineer}.md` — added by
  the team port (upstream doesn't ship these roles).

Tests live outside `starter/` under [`../tests/lua/`](../tests/lua/)
so the installed config has no test code.

## Run modes (NEFOR_DEV_DIR)

The team's init.lua handles two run modes by reading `NEFOR_DEV_DIR`:

- **Dev mode** (`NEFOR_DEV_DIR=/path/to/personal/nefor`) — every
  `pm.install` spec carries a `dir = NEFOR_DEV_DIR/<sub-path>` override
  so pm skips the github clone path and registers the local checkout
  directly. `STARTER_UPSTREAM = NEFOR_DEV_DIR/starter` lands on
  package.path so `require("agentic-loop")` and the upstream
  compositors resolve against the local checkout.

- **Prod mode** (`NEFOR_DEV_DIR` unset) — `bootstrap_pm` clones the
  upstream nefor repo (sparse, `lua` + `starter` + `plugins`) into
  `$NEFOR_DATA_DIR/nefor/`, and `pm.install` fetches the per-plugin
  Lua subtrees pinned to `UPSTREAM_TAG` (currently `v0.1.5`).

**Caveat:** upstream's `nefor-pm` in `v0.1.5` doesn't fully reconcile
the sparse-checkout layout with `package.path` resolution for cloned
plugins (the sparse subtree paths land at
`<plugin_dir>/plugins/<name>/lua/<name>/init.lua`, but `pm` grafts
`<plugins_root>/?/init.lua` which expects a flatter layout). Dev mode
works today; prod mode needs an upstream pm fix before this config
can boot on a fresh machine without `NEFOR_DEV_DIR`.

## Sync procedure (when upstream advances)

1. Bump `UPSTREAM_TAG` in `init.lua` to the new upstream release tag.
2. Inspect the upstream diff at that tag for shape changes to the
   openai-provider plugin lib (`plugins/openai-provider/lua/...`),
   the actor-spec shape consumed by `pm.install`-resolved
   compositors, and the lead-workflow / agentic-loop public API the
   team's `init.lua` calls into.
3. Apply any team-side overlay changes to
   `compositors/qwen_hooks.lua` if the lib's translation primitives,
   the upstream provider compositor's hooks contract, or the kind
   names changed.
4. Re-test: `NEFOR_CONFIG=mock just run-dev <upstream-checkout>` first
   (deterministic), then `NEFOR_CONFIG=test` (ollama), then prod.
