# nefor-team

Internal team configuration for running [nefor](https://github.com/amenocturne/nefor)
against the corp Nestor LLM gateway. Everything outside `starter/` is
build/run scaffolding; the actual config lives in
[`starter/init.lua`](starter/init.lua).

## What this is

`nefor` is the OSS engine: a Lua-composable plugin host that connects a
TUI chat surface to an OpenAI-compatible model provider. This repo is
the team-internal **consumer config** on top of that — DP authentication
against Nestor, JWT exchange, Nestor-specific provider config, the lead
orchestrator role, and a think-tag filter that splits inline
`<think>...</think>` chain-of-thought into a separate reasoning event
type. None of the internal pieces fork the engine; we configure it.

This repo is a plain `nefor-pm` consumer — upstream nefor is fetched
via the `amenocturne/nefor` package manager on first boot, and only
the team-specific files live in this repo (see
[`starter/README.md`](starter/README.md) for the inventory).

## Prerequisites

1. **Public nefor**, built or installed:

       brew install amenocturne/tap/nefor

2. **Public nefor with the `--auth-header` flag on `openai-provider`.**

   Nestor gates on `Nestor-Token: <jwt>` rather than the standard
   `Authorization: Bearer ...`, so `openai-provider` needs the
   `--auth-header NAME` flag (default `Authorization`) the team starter
   uses to override that header. The flag landed on the
   `openai-provider-auth-header` branch in public nefor; if you're on
   an older release that pre-dates it, build that branch locally or
   wait for the next tagged release. Without the flag, nefor exits
   immediately at startup with `error: unexpected argument '--auth-header'`.

3. **DP CLI** installed. See `<INTERNAL_DP_DOCS_URL>` for the install
   instructions. The starter looks for `dp` at:

   - `/usr/local/bin/dp` (system install)
   - `~/.nessy/dp_v13.4.2/dp` (nessy-managed install)
   - anywhere on `$PATH` (last-resort lookup via `command -v dp`)

4. **Logged into DP at least once.** Run

       dp auth login

   in a terminal and complete the browser flow before starting nefor.
   The team starter does not currently invoke an interactive `dp auth
   login` for you — if no DP session exists at startup, nefor exits
   with a message telling you to run `dp auth login` and restart.

5. **`curl`** on `$PATH`. The starter shells out to curl for HTTP
   (token exchange + model list fetch); mlua's safe stdlib doesn't
   ship an HTTP client and the engine's only async subprocess API is
   incompatible with synchronous startup auth.

6. **`da`** on `$PATH` ([github.com/amenocturne/da](https://github.com/amenocturne/da)).
   Upstream's `tool-validator` runs every `bash` invocation through `da`
   so safe read-only commands skip the approval popup. `just install`
   (this repo) chains into upstream's `just install-nefor`, which
   installs it for you (`cargo install --locked dabin`); if you skip
   the install and `da` isn't on `$PATH`, the validator degrades to
   "always defer to user" — popup-spammy but safe.

## Install

Two install styles, pick whichever matches how you usually work:

### Recommended: `just install`

```sh
git clone <INTERNAL_GIT_URL> nefor-team
cd nefor-team
just install
```

`just install` is a composite of two recipes you can also run independently:

- `just install-nefor` — chains into upstream's `just install-nefor`
  (binaries to `~/.local/bin`, `da` via cargo install). Re-run after
  pulling fresh upstream changes to refresh binaries; never touches
  your config.
- `just install-starter` — copies this checkout's `starter/` to
  `~/.config/nefor`. Refuses if the dir already exists (your config is
  yours — re-copying would clobber tweaks). Pass `force` to wipe and
  re-copy: `just install-starter force`.

Pre-req: `NEFOR_UPSTREAM` either points at your nefor checkout (e.g.
a sibling clone — that's the default) or you've exported it in your
shell. The recipe shells into that directory to run upstream's
install path.

### Dev mode (run against the repo checkout)

```sh
git clone <INTERNAL_GIT_URL> nefor-team
cd nefor-team
just run
```

`just run` invokes `nefor --config $PWD/starter`, so any edits in
`starter/` take effect on the next launch — no copy step.

### Dev mode with a local upstream checkout

If you're iterating on upstream nefor alongside the team config:

```sh
just run-dev /path/to/personal/nefor
```

This sets `NEFOR_DEV_DIR`, which makes the team's `init.lua`
short-circuit `nefor-pm`'s github fetch path and resolve every
upstream dependency (core-libs, plugin libs, starter overlay, chat.lua)
against your local checkout. Useful when landing an upstream fix and
the team-side adaptation in the same session.

## Run

```sh
just run        # if running from the repo
nefor           # if you copied to ~/.config/nefor/
```

Startup is synchronous: the engine reads `init.lua`, which
authenticates against Nestor (DP token -> JWT), fetches the model
list, picks a model, and only then spawns the plugins. The TUI comes
up after the JWT is in hand. Expect a 1-3s pause at first launch
while the network round trips happen.

The startup banner (printed to stderr; visible if you launched with
`just run` or `RUST_LOG=info nefor --config ...`) shows:

    [nefor-team] authenticating against Nestor (https://code-completion-nestor.tcsbank.ru)...
    [nefor-team] Nestor JWT acquired.
    [nefor-team] Nestor models available: 7
    [nefor-team]   - qwen3.5 — Qwen 3.5 long-context
    [nefor-team]   - gpt-4o — GPT-4o
    ...
    [nefor-team] using model: qwen3.5
    [nefor-team] (to switch: edit init.lua or set NEFOR_TEAM_MODEL=<name> and restart)

## DP auth flow

1. `init.lua` calls `auth.get_jwt()` at boot.
2. `get_jwt` finds the `dp` binary, runs `dp auth print-token`, gets
   a DP bearer token.
3. It POSTs the DP token to
   `https://code-completion-nestor.tcsbank.ru/api/v2/token`. Nestor
   returns `{ jwt, token: { expires_at } }`.
4. The JWT is held in module-local memory. No disk cache. If it
   expires, the next `get_jwt()` re-runs the exchange — but in v0.1
   nothing calls `get_jwt()` after startup, so practically: when your
   JWT expires, restart nefor.
5. The JWT is passed to `openai-provider` on the command line via
   `--api-key <jwt>`. `openai-provider` then sends it on every chat
   request as `Nestor-Token: <jwt>` (because of `--auth-header
   Nestor-Token`).

If `dp auth print-token` reports no active session (you've never
logged in or the session expired), nefor exits with:

    DP authentication required. Run `dp auth login` in another
    terminal, then restart nefor.

Run `dp auth login`, complete the browser flow, then `just run` (or
`nefor`) again. DP's session is independent of nefor's process; once
it's active, every nefor restart picks it up automatically.

## Models

Available models are fetched from
`/api/v1/cli/models` on every startup and printed to the banner.

To switch which model nefor uses:

- **Quick:** set `NEFOR_TEAM_MODEL=<model-name>` in your shell, then
  restart nefor. The starter reads this env var on boot and uses it
  if non-empty.
- **Permanent:** edit the `pick_model()` body in `starter/init.lua` to
  hard-code the name, or change the preference order, then restart.

Mid-session model switching is not supported in v0.1. The model name
is part of the spawned `openai-provider` argv; changing it would
require respawning the provider plugin, which the engine doesn't
expose a clean Lua API for yet.

## Troubleshooting

### `DP authentication required.` on startup

Run `dp auth login` in another terminal, then restart nefor. If `dp
auth print-token` succeeds in your terminal but nefor still reports
this, check that `DP_WORKDIR` (or its absence) is consistent — the
starter sets `DP_WORKDIR=~/.nessy/dp_v13.4.2` only when that path
exists; if you have both a system `dp` and a nessy-managed `dp`, the
system one wins.

### `dp CLI not found`

Install DP per `<INTERNAL_DP_DOCS_URL>`. The starter looks at
`/usr/local/bin/dp`, `~/.nessy/dp_v13.4.2/dp`, and `$PATH`.

### `token-exchange-failed: status=...`

The DP token was obtained but Nestor rejected the exchange. Common
causes:

- Network unreachable (VPN, corp network policy). Try
  `curl -v https://code-completion-nestor.tcsbank.ru/api/v2/token`
  with the same headers manually.
- DP token expired between `dp auth print-token` and the POST. Run
  `dp auth login` and retry.
- Nestor backend issue. Check the team channel.

### nefor exits with `error: unexpected argument '--auth-header'`

The installed `openai-provider` pre-dates the `--auth-header` flag.
Build public nefor from the `openai-provider-auth-header` branch (or
a release that includes it) and reinstall; brew will pick up the new
binary on the next `brew upgrade nefor`.

### `NEFOR_PLUGIN_DIR is not set` on startup

The engine normally sets this for you — brew-installed nefor points
it at the formula's plugin tree, and `nefor --plugin-dir <path>`
overrides explicitly. If you see this error, you're either running
the engine binary directly without going through the launcher
wrapper, or running a dev checkout without exporting it. Pass
`--plugin-dir` or set the env var to your build's plugin output
directory.

### Models list is empty in the banner

The JWT was obtained but the models endpoint returned nothing
parseable. The starter still picks a fallback (`NEFOR_TEAM_MODEL`,
the literal `"default"`, etc.) and proceeds. Check the engine log
(`tail -f starter/nefor.log`) for the exact response.
