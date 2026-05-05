# nefor

> Agent harness substrate.

A pure string-bus engine plus separate-process plugins (NCP v0.1 over JSON-line stdio), with Lua composition wiring it all together. Build chat surfaces, providers, schedulers, and tools as plugins in any language.

**Status: Stage 1 — initial public release.** End-to-end chat works against any OpenAI-compatible provider (Ollama by default), with a declarative TUI, a typed reasoner-graph scheduler, tool-gate permissioning, and Lua-side session persistence.

## Install

```sh
brew install amenocturne/tap/nefor
```

Then scaffold a config in your XDG config dir:

```sh
mkdir -p ~/.config/nefor
cp $(brew --prefix)/share/nefor/starter/*.lua ~/.config/nefor/
```

The starter config talks to `localhost:11434` (Ollama default). Edit `~/.config/nefor/init.lua` to change provider / model.

## Quick start

```sh
nefor
```

Launches the chat TUI. `Ctrl+C` or `/quit` exits; `/new` clears the transcript.

## Architecture

The engine is a pure string-layer event bus: it reads plugin stdin, stamps `{origin, ts}`, persists to a session log, and invokes a required Lua `dispatch` hook. NCP v0.1 (handshake, broadcast, replay, errors) lives entirely in Lua under `starter/ncp.lua`. Plugins are independent OS processes communicating via JSON lines.

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
- `starter/init.lua`, `starter/chat.lua`, `starter/ncp.lua` — default composition, chat surface, NCP-in-Lua.

### Paths

`nefor` resolves config and data via XDG-style env vars:

| Env var            | Default                          | Holds       |
|--------------------|----------------------------------|-------------|
| `NEFOR_CONFIG_DIR` | `$XDG_CONFIG_HOME/nefor`         | `init.lua`  |
| `NEFOR_DATA_DIR`   | `$XDG_DATA_HOME/nefor`           | sessions    |
| `NEFOR_PLUGIN_DIR` | `$NEFOR_DATA_DIR/plugins`        | binaries    |

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
