<h1 align="center">Nefor</h1>

<p align="center"><i>A hyperextensible runtime for composing tools, plugins, reasoners, and interfaces in Lua.</i></p>

Small core. User-owned config. Replaceable everything else.

Nefor is closer to Neovim than to a fixed agent application: the engine runs
process plugins, routes their messages, hosts Lua, and stamps identity. A Lua
`init.lua` chooses the providers, tools, policies, persistence, orchestration,
and interface that make up a distribution.

The engine does not require an LLM. Models, scripts, tools, agents,
orchestrators, and plain interfaces are all units you can compose. The bundled
starter is a working agentic TUI and a reference distribution, not the product
boundary.

## Why Nefor

Most tools expose selected extension points and eventually leave an important
part fixed. Nefor puts the integration layer in user-owned Lua instead. Plugins
are separate processes over a line-oriented protocol, interfaces are ordinary
composition, and policy remains readable and replaceable.

The starter demonstrates that shape with a chat surface, sessions, permission
gates, providers, tools, and MAG workflows. Keep it, alter it, or replace it.

## Quick start

The credential-free path builds Nefor and installs a copy of the starter:

```sh
git clone https://github.com/amenocturne/nefor.git
cd nefor
just install
nefor
```

The starter defaults to the deterministic `mock-plugin` / `mock-model`. No API
key or provider login is needed. Type a message in the TUI to exercise the full
local workflow.

`just install` does not overwrite an existing `~/.config/nefor`. See the
[installation guide](docs/user/installation.md) for release channels, Homebrew,
platform support, paths, and safe config handling.

## What you can compose

- **Tools and plugins** — run independent binaries over stdio, then gate, wrap,
  translate, or replace them.
- **Interfaces** — put a TUI, CLI, bridge, or custom surface over the same
  runtime.
- **Reasoners and workflows** — combine model calls, scripts, tools, agents, or
  orchestrators; the starter uses typed MAG programs.
- **Policy** — own approvals, routing, persistence, replay, and provider choice
  in Lua.
- **Distributions** — ship a complete composition or maintain a private config
  that fits one environment.

## Documentation

Start with the [documentation index](docs/index.md):

- [Getting started](docs/user/getting-started.md)
- [Installation](docs/user/installation.md)
- [Troubleshooting](docs/user/troubleshooting.md)
- [Architecture](docs/architecture.md)
- [Plugin authoring](docs/plugin-authoring.md)
- [Protocol](docs/protocol.md)

The [manifesto](docs/manifesto.md) states the project's design commitments.

## Development

Run `just` to discover the repository's command surface. The usual local checks
are `just check`; broader test groups are documented in
[docs/testing.md](docs/testing.md).

## License

[MIT](LICENSE)
