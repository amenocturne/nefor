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
| `--data-dir <DIR>`       | Writable runtime data root.                                                   |\n| `--log-file <PATH>`      | Exact aggregate log file path.                                                |
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


Aggregate logging destination:\n\n1. non-empty `NEFOR_LOG_STDERR` selects stderr\n2. `--log-file` selects that exact path\n3. `NEFOR_LOG_FILE` selects that exact path\n4. `<data-root>/logs/nefor.log`\n\nExplicit file paths are not relocated under the data root. The engine creates parent directories and exits with a diagnostic naming the selected path if it cannot initialize logging.\n\nPlugin executable and immutable runtime roots are selected by the active Lua distribution helper, not by the engine.

The engine exports resolved `NEFOR_CONFIG_DIR` and `NEFOR_DATA_DIR` before Lua composition and subprocess spawn. Plugin commands are resolved by Lua/distribution code.

## Version output
