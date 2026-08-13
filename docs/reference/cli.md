# CLI reference

This page distinguishes arguments parsed by the engine, arguments forwarded to the selected Lua composition, and virtual CLIs registered by that composition.

## Engine grammar

```text
nefor [GLOBAL_OPTIONS]
nefor [GLOBAL_OPTIONS] run [CONFIG_ARG ...]
nefor [GLOBAL_OPTIONS] plugin
nefor [GLOBAL_OPTIONS] plugin <NAME> [PLUGIN_ARG ...]
```

Global options:

| Option                   | Meaning                                                                       |
| ------------------------ | ----------------------------------------------------------------------------- |
| `--config <DIR>`         | Configuration directory containing `init.lua`.                                |
| `--data-dir <DIR>`       | Writable runtime data root.                                                   |
| `--plugin-dir <DIR>`     | Runtime plugin executable root.                                               |
| `-h`, `--help`           | Engine help.                                                                  |
| `-V`, `--version`        | Build version.                                                                |

Bare `nefor` and `nefor run` both start serve mode. Only `run` forwards trailing arguments to Lua as `nefor.runtime.argv`. Every token after `run`, including a hyphenated token, belongs to the composition; put engine options before `run`.

```sh
nefor --config ./my-config run --session abc --mode safe
```

`nefor --session abc` is invalid because `--session` is not an engine option.

## Forwarded composition arguments

Arguments after `run` are opaque strings owned by the selected `init.lua`. The engine imposes no syntax or meaning on them.

## Virtual plugin CLIs

`nefor plugin` lists `cli` entries registered while loading the selected config. `nefor plugin <NAME> ...` calls that Lua entry and forwards the remaining argv. Clap consumes a standalone `--`; because the engine intercepts outer help, a virtual CLI that needs its own `--help` may require:

```sh
nefor plugin <name> -- --help
```

### Development-only `agentic-cli`

`agentic-cli` is registered by the repository's `cli-config/` for deterministic development and tests. It is not included in the shipped starter, release archive, or Homebrew distribution. Do not present `nefor plugin agentic-cli` as an installed end-user command unless the active config explicitly registers it. Its current usage is documented in [`lua/libs/cli/README.md`](../../lua/libs/cli/README.md).

## Directory resolution

Configuration:

1. `--config`
2. `NEFOR_CONFIG_DIR`
3. `$XDG_CONFIG_HOME/nefor`, otherwise `~/.config/nefor`

Data:

1. `--data-dir`
2. `NEFOR_DATA_DIR`
3. `$XDG_DATA_HOME/nefor`, otherwise `~/.local/share/nefor`

Plugin executable root:

1. `--plugin-dir`
2. `NEFOR_PLUGIN_DIR`
3. executable directory when it contains the bundled plugin set
4. `<data-root>/bin` when it contains the bundled plugin set
5. the executable's adjacent installed `share/nefor/plugins`
6. an existing `$NEFOR_DATA_DIR/plugins`
7. `$XDG_DATA_HOME/nefor/plugins`, otherwise `~/.local/share/nefor/plugins`

The engine exports resolved `NEFOR_CONFIG_DIR`, `NEFOR_DATA_DIR`, and `NEFOR_PLUGIN_DIR` before Lua composition and subprocess spawn.

## Version output
