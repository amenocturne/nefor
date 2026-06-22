<h1 align="center">nefor</h1>

<p align="center"><i>Hyperextensible runtime for composing tools, plugins, and interfaces in Lua.</i></p>

Small core. User-owned config. Replaceable everything else.

Think Neovim as a runtime: a programmable core, process plugins, and a Lua
config you own. The engine spawns processes, routes lines, hosts Lua, and stamps
identity. Your `init.lua` decides what exists, how it talks, what gets
persisted, and which interface sits on top.

Nefor does not require LLMs. Models, scripts, tools, agents, orchestrators, and
plain interfaces are composable units when you wire them in. The bundled starter
is one distribution, not the product boundary.

## Why

Most tools expose selected extension points. Eventually you hit the wall: this
part is configurable, that part is not.

Nefor moves the wall into Lua. Plugins are independent OS processes. Interfaces
are another composition. Routing, persistence, orchestration, approvals, and
protocol semantics live where you can read and rewrite them.

The starter proves the shape with a chat surface, providers, tool gates,
sessions, and workflow actors. Keep it, strip it down, or use it as a reference
for your own distribution.

## What You Can Compose

- **Tools:** spawn them as plugins, gate them, wrap them, translate them, or
  replace them.
- **Plugins:** run independent binaries over stdio. Rust is common here; the
  boundary is process + lines.
- **Interfaces:** put a TUI, CLI, bridge, or custom surface on the same runtime.
- **Reasoners:** compose LLM calls, scripts, tool calls, agents, orchestrators,
  or any unit that reads context and produces work.
- **Policies:** own approval, routing, persistence, replay, provider choice, and
  dispatch behavior in Lua.
- **Distributions:** ship a complete `init.lua` with plugins and defaults, or
  keep a private config that only fits your machine.

## Install

From source:

```sh
git clone https://github.com/amenocturne/nefor
cd nefor
just install
```

`just install` builds the engine and plugin binaries, then copies the starter
composition to `~/.config/nefor`. Use lower-level targets when needed:

```sh
just install-nefor source     # source | latest | nightly
just install-starter safe     # safe | force
```

`install-starter` refuses to overwrite an existing config unless you pass
`force`.

Or install the engine with brew:

```sh
brew install amenocturne/tap/nefor
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/starter/* ~/.config/nefor/
```

The starter ships with a deterministic offline mock provider, plus
`openai-provider` for OpenAI-compatible APIs and `chatgpt-provider` for the
ChatGPT Responses API.

## Quick Start

Run the starter:

```sh
nefor
```

The first run uses the mock provider, so no live model is required. Edit the
copied config for real providers, different tools, or different wiring:

```sh
$EDITOR ~/.config/nefor/config/init.lua
$EDITOR ~/.config/nefor/init.lua
```

A Nefor composition is an `init.lua`. Use the starter as the concrete reference
for provider setup, tool gating, session replay, workflow actors, and TUI
wiring.

## Architecture

The engine spawns processes and routes lines through Lua. Everything else is
composition.

| Layer                                             | What it owns                                                                                                           |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Engine / bus**                                  | Process spawning, line routing, Lua hosting, and identity stamping (`origin`, `ts`). It does not parse message bodies. |
| **Plugins** (`plugins/`)                          | Self-contained work over stdio. Each plugin owns one scoped task.                                                      |
| **Lua config / starter** (`init.lua`, `starter/`) | Dispatch hooks, actor spawning, policies, persistence, provider/tool wiring, and interfaces.                           |
| **Interfaces**                                    | User surfaces composed over the same bus. The starter uses `nefor-tui`; you can wire another.                          |

Bash-tool test: a plugin should feel like a self-contained utility you could run
from a shell, then compose elsewhere. Plugins should not know their neighbors;
composition belongs in Lua.

## Development

All commands live in the [`justfile`](justfile). Run `just` to see the full list.

## Docs

- [Architecture and writing principles](docs/principles.md)
- [Plugin authoring guide](docs/plugin-authoring.md)
- [Testing](docs/testing.md)
- [Glossary](docs/glossary.md)
- [NCP spec](protocol/v0.1/spec.md)
- [Plugins](plugins/README.md)
- [Starter composition](starter/README.md)
- [Lua core](lua/core/README.md)

## License

Do whatever you want, i.e. [MIT](./LICENSE).
