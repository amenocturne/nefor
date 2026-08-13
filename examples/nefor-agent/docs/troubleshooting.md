# Troubleshooting

Start with the visible symptom. Unless an error says otherwise, inspect
`<config-dir>/nefor.log`; the default is `~/.config/nefor/nefor.log`.

## `nefor: no init.lua found`

The engine resolved a config directory that does not contain `init.lua`.

1. Check whether `NEFOR_CONFIG_DIR`, `XDG_CONFIG_HOME`, or `--config` points
   somewhere unexpected.
2. For a Homebrew install, scaffold the starter as shown in
   [Installation](installation.md#homebrew).
3. For a checkout install, rerun `just install-example safe`. It preserves an
   existing config directory; if that directory is incomplete, inspect it
   before choosing the destructive `force` mode.

You can bypass discovery to verify a config directly:

```sh
nefor --config /absolute/path/to/config
```

## `nefor: command not found`

The source installer puts the command in `${PREFIX:-$HOME/.local}/bin`. Add
that directory to `PATH`, open a new shell, and verify:

```sh
export PATH="${PREFIX:-$HOME/.local}/bin:$PATH"
nefor --version
```

For Homebrew, use `brew --prefix` and `brew doctor` to diagnose the package
manager's command path.

## Plugins fail to spawn or the engine cannot resolve a plugin root

A runtime binary and its plugin set are missing or from different layouts.
Avoid pointing `NEFOR_EXECUTABLE_ROOT` at the Lua source overlay under
`~/.local/share/nefor/plugins`; source installs place executables in
`~/.local/share/nefor/bin`.

- Reinstall the complete runtime with `just install-nefor source`, `latest`, or
  `nightly`.
- Remove stale `NEFOR_EXECUTABLE_ROOT` overrides unless they are intentional.
- If using explicit paths, pass the directory containing executables such as
  `nefor-tui`, `mock-plugin`, and `mag-plugin`.
- Read `nefor.log`. After the TUI closes, abnormal plugin exits are also
  reported on stderr with the log path.

## First launch asks for credentials or uses ChatGPT unexpectedly

The shipped starter does not do this: its defaults are `mock-plugin` and
`mock-model`. You are running a modified or older config, or environment
variables override it.

1. Inspect `~/.config/nefor/config/init.lua` (or your resolved config path).
2. Check `NEFOR_DEFAULT_PROVIDER`, `NEFOR_DEFAULT_MODEL`,
   `NEFOR_ENABLE_CHATGPT`, and `NEFOR_ENABLE_OLLAMA`.
3. Compare the config with the `examples/nefor-agent/` directory from the same Nefor
   version. Do not force-replace it until you have saved local changes.

## The TUI starts, but the mock response is not useful

That is expected. The mock is deterministic and exists to verify startup,
routing, tools, and the interaction surface without credentials. Enable and
configure a real provider only after the mock path works; see
[Getting started](getting-started.md#4-customize-the-copied-composition).

## A real provider appears, but requests fail

Provider enablement and provider authentication are separate.

- **ChatGPT:** run the `chatgpt-provider login` binary from the resolved plugin
  directory, then restart Nefor. OAuth state is stored under the resolved Nefor
  data directory.
- **Ollama:** ensure an OpenAI-compatible server is listening at
  `http://localhost:11434` and the selected model exists. The starter uses the
  static placeholder token `ollama-local`; it does not require a cloud key.
- Use `/model` to select a model registered by an enabled provider.

Provider HTTP and model-capability errors should appear in the transcript; the
engine and plugin details remain in `nefor.log`.

## `nefor --session ...` is rejected

Session, prompt, and mode arguments belong to the starter's `run` subcommand:

```sh
nefor run --session <id>
nefor run --prompt "hello"
nefor run --mode safe
```

Engine flags such as `--config` and `--data-dir` may appear before `run`.

## Tools operate in the wrong directory

Plugins inherit the directory from which the engine was launched. Change into
the intended project before starting Nefor:

```sh
cd /path/to/project
nefor
```

Changing the config or plugin directory does not change the tools' working
directory.

## An update did not change my starter behavior

Runtime and config updates are intentionally independent. `brew upgrade` and
`just install-nefor ...` update binaries but preserve `~/.config/nefor`.
Compare your config with the new release's `examples/nefor-agent/`. The only built-in full
replacement is `just install-example force`, which deletes the existing config
before copying the new starter.

## A session does not resume across versions

Before the project has public compatibility guarantees, compatibility is only
promised within one minor line: `0.y.x` remains compatible with `0.y.0`.
Cross-minor config, wire-format, and old-session compatibility is not promised.
Keep the originating distribution available when an old session matters; see
[session documentation](sessions.md).

## Nix or Home Manager behaves differently

The flake and Home Manager module are experimental rather than a primary,
release-verified installation route. Reproduce the issue with a supported
release artifact or source install first; that separates packaging behavior
from engine or starter behavior.
