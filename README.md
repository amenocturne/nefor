<h1 align="center">nefor</h1>

<h3 align="center"><i>More than an agentic harness</i></h3>

## Why

Every harness is a monolith with boundaries. Boundaries on what you can replace,
what you can extend, what you can disable. The answer to "can I change X?" is
always "it depends on whether X is exposed."

Nefor has no boundaries because there is nothing to expose. The engine is a
process spawner with a Lua VM. Plugins are independent OS processes that read
and write lines. Your `init.lua` wires them together. Replace any plugin.
Rewrite any wiring. Remove anything you don't need. The design principle is not
"extensible" — it's "do whatever you want."

The default starter config ships an agent harness: chat surface, orchestrator,
permission gates, session persistence. That's one composition. Not the only one.

## Install

```sh
git clone https://github.com/amenocturne/nefor
cd nefor
just install-nefor
just install-starter
```

`install-nefor` builds and installs the engine + all plugin binaries. Accepts a
channel: `source` (default, builds locally), `latest` (brew or stable tarball),
`nightly` (rolling tarball from main).

`install-starter` copies the default composition to `~/.config/nefor`. Refuses
if the dir exists — your config is yours. Pass `force` to overwrite.

Or via brew:

```sh
brew install amenocturne/tap/nefor
```

Then copy the starter config from the formula's share directory:

```sh
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/starter/* ~/.config/nefor/
```

The starter ships with a mock provider (deterministic, works offline, useful for
learning the system), plus `openai-provider` (Ollama, Groq, OpenRouter, vLLM,
OpenAI) and `chatgpt-provider` (ChatGPT Responses API).

## Quick start

Just run `nefor` and discover features as you go. The mock provider shows you
the basics without needing a live model. For real providers, see the config.

## Architecture

The engine spawns processes and routes lines between them through a Lua
`dispatch` hook. That is the entire commitment. Everything else — protocol
semantics, session persistence, orchestration, the chat surface — lives in Lua
or in plugins.

| Layer | What it owns |
|-------|-------------|
| **Engine** | Process spawning. Line routing. Lua VM hosting. Identity stamping (`origin`, `ts`). |
| **Lua** (`lua/core/`, `starter/`) | NCP protocol. Broadcast semantics. Session persistence. Composition wiring. All opinion. |
| **Plugins** (`plugins/`) | The actual work. Each a self-contained process communicating over stdio. |

## Development

All commands live in the [`justfile`](justfile). Run `just` to see the full
list.

## Docs

Repo-wide:

- [Architecture and writing principles](docs/principles.md)
- [Plugin authoring guide](docs/plugin-authoring.md)
- [Testing](docs/testing.md)
- [Glossary](docs/glossary.md)
- [NCP spec](protocol/v0.1/spec.md)

Per-component docs live in each package's README. See
[plugins/](plugins/README.md), [starter/](starter/README.md),
[lua/core/](lua/core/README.md).

## FAQ

**Why Lua?** Composition is configuration. Configuration should be a real
language. Lua is the lightest one that embeds cleanly. Same reasoning Neovim
applied.

**Why not [framework X]?** Frameworks own the composition layer. Nefor does not
— your `init.lua` does. If you disagree with a design choice, replace the plugin
or rewrite the wiring.

**Can I use this without LLMs?** Yes. The engine routes lines between processes.
It does not know what an LLM is. A model is one kind of plugin among many.

## License

Do whatever you want, i.e. [MIT](./LICENSE).
