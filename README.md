# nefor

> Agent harness substrate.

A pure string-bus engine plus separate-process plugins (NCP v0.1 over JSON-line stdio), with Lua composition wiring it all together. Build chat surfaces, providers, schedulers, and tools as plugins in any language.

**Status: Stage 1 — initial public release.** End-to-end chat works against any OpenAI-compatible provider (Ollama by default), with a declarative TUI, a typed reasoner-graph scheduler, tool-gate permissioning, and Lua-side session persistence.

## Install

### From a checkout (recommended)

```sh
git clone https://github.com/amenocturne/nefor
cd nefor
just install
```

`just install` is a composite of two recipes you can also run independently. It accepts a `channel` argument (`source`, `latest`, `nightly`) that forwards to `install-nefor`. Default for now is `source` until the brew formula + nightly tag pipeline is set up.

- `just install-nefor [channel]` — builds the workspace (channel `source`), drops the `nefor` binary into `~/.local/bin` (the only thing on PATH), and lands every plugin binary plus `da` (the bash-command classifier the tool-validator uses, installed via `cargo install --root <libexec> dabin`) into `~/.local/share/nefor/bin`. The engine resolves plugins from there by default — no `NEFOR_PLUGIN_DIR` export required. Re-run after pulling to refresh binaries; never touches your config.
- `just install-starter` — copies the in-repo `starter/` to `~/.config/nefor` so a bare `nefor` from any cwd picks it up. Refuses if the dir already exists (your config is yours — re-copying would clobber local tweaks). Pass `force` to wipe and re-copy from this checkout: `just install-starter force`.

### From brew

```sh
brew install amenocturne/tap/nefor
```

Brew doesn't install `da` (cargo-only at the moment) and doesn't drop the starter config. Either run `just install` from a checkout once to wire those up, or do it manually:

```sh
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/starter/* ~/.config/nefor/
cargo install --locked dabin
```

The starter config talks to `localhost:11434` (Ollama default). Edit `~/.config/nefor/init.lua` to change provider / model.

## Bash safety

The starter ships a `tool-validator` actor that classifies every `bash` invocation through [`da`](https://github.com/amenocturne/da) before any approval popup appears. The validator owns three outcomes:

- `da` exits 0 (approve) — the call is auto-approved; no popup. Covers read-only binaries (`ls`, `find`, `grep`, …), `--help`/`--version` for any binary, `git read,add,commit,restore-staged,tag,fetch,pull,push`, `cargo local` operations.
- `da` exits 2 (deny) — the call is auto-denied; no popup.
- `da` exits 1 (defer) or `da` is absent — defers to a user popup. The popup is the only way the human sees a permission prompt; tool-gate's `chat.tool.permission_request` never reaches the chat surface directly.

`just install-nefor` installs `da` automatically (`cargo install --locked dabin`). If the binary isn't on `PATH` the validator logs a warning at startup and falls back to "always defer" — safe by construction.

To change the policy stack (e.g. add `--cargo crates-publish` for a release pipeline), edit `DA_ARGS` in `starter/tool-validator/init.lua`.

## Quick start

```sh
nefor
```

Launches the chat TUI. `Ctrl+C` or `/quit` exits; `/new` clears the transcript.

## Architecture

The engine is a pure string-layer event bus: it reads plugin stdin, stamps `{origin, ts}`, persists to a session log, and invokes a required Lua `dispatch` hook. NCP v0.1 (handshake, broadcast, replay, errors) lives entirely in Lua under `lua/core/`. Plugins are independent OS processes communicating via JSON lines.

### Layout

- `crates/nefor/` — engine binary (NCP broker + mlua host).
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types.
- `crates/nefor-combinators/` — in-process algebra library.
- `plugins/nefor-tui/` — declarative TUI plugin: layout primitives + reconciler + line-diff renderer + Lua VM. Hosts the chat surface as a Lua composition.
- `plugins/openai-provider/` — OpenAI-compatible HTTP provider.
- `plugins/reasoner-graph/` — typed graph scheduler with cycles, per-firing lifecycle, fanout combinators.
- `plugins/tool-gate/`, `plugins/basic-tools/` — tool advertisement and permission gate; bundled tools.
- `plugins/generic-provider/`, `plugins/generic-tool/` — passive type-registry hubs.
- `plugins/nefor-combinators/` — typed combinator registry plugin.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests.
- `starter/` — default composition (`starter/init.lua`), chat surface (`starter/chat/`), per-actor folders.
- `lua/core/` — shared protocol primitives (NCP v0.1, actor runtime, history replay).

### Paths

`nefor` resolves config and data via XDG-style env vars:

| Env var            | Default                          | Holds       |
|--------------------|----------------------------------|-------------|
| `NEFOR_CONFIG_DIR` | `$XDG_CONFIG_HOME/nefor`         | `init.lua`  |
| `NEFOR_DATA_DIR`   | `$XDG_DATA_HOME/nefor`           | sessions    |
| `NEFOR_PLUGIN_DIR` | `$NEFOR_DATA_DIR/bin`            | binaries    |

CLI flags (`--config`, `--data-dir`, `--plugin-dir`) override env vars.

## Build from source

```sh
just setup
just run     # debug build, in-tree dev mode
just test
just lint
```

## License

MIT — see `LICENSE`.
