# Getting started

This path starts the bundled agentic TUI without credentials or an external
model. For other channels and prerequisites, see [Installation](installation.md).

## 1. Install the engine and starter

```sh
git clone https://github.com/amenocturne/nefor.git
cd nefor
just install
```

This source-channel install builds the workspace, puts the `nefor` and `mag`
commands under `${PREFIX:-$HOME/.local}/bin`, installs plugin binaries under
`~/.local/share/nefor/bin`, and copies the starter to `~/.config/nefor`.
Ensure the command directory is on `PATH`:

```sh
export PATH="${PREFIX:-$HOME/.local}/bin:$PATH"
```

The installer leaves an existing config untouched. That protects edits, but it
also means an old starter is not silently upgraded; see
[Updating](installation.md#updating).

## 2. Launch Nefor

Run it from the directory in which you want tools to operate:

```sh
cd /path/to/your/project
nefor
```

Plugins inherit this working directory. The starter opens its TUI and uses the
local deterministic defaults:

```text
provider: mock-plugin
model:    mock-model
```

No API key, network model, or ChatGPT login is required. Enter a message to run
the starter workflow. The mock response is scripted, so this path verifies the
installation and interaction flow rather than model quality.

## 3. Choose an operating mode

The default is safe mode. Startup arguments belong after `run`:

```sh
nefor run --mode safe
nefor run --mode auto
nefor run --yolo
nefor run --prompt "Inspect this repository"
```

`--yolo` is shorthand for `--mode yolo`. Read the [permissions guide](permissions.md) before relaxing permissions.

## 4. Customize the copied composition

The two main entry points are:

```text
~/.config/nefor/init.lua
~/.config/nefor/config/init.lua
```

`init.lua` owns wiring; `config/init.lua` owns the starter's provider and policy
settings. The starter keeps ChatGPT, local Ollama, and OpenRouter opt-in. To expose them
without editing the config:

```sh
NEFOR_ENABLE_CHATGPT=1 nefor
NEFOR_ENABLE_OLLAMA=1 nefor
OPENAI_PROVIDER_API_KEY=... NEFOR_ENABLE_OPENROUTER=1 nefor
```

ChatGPT additionally requires the provider's OAuth login, OpenRouter uses the generic
provider key input shown above, and Ollama requires a
running OpenAI-compatible endpoint at `http://localhost:11434`. These are not
part of the credential-free verification path. Use the TUI's `/model` command
to select among enabled providers and models.

## Next steps

1. Learn the [TUI](../../../plugins/nefor-tui/README.md) and its [commands and keys](../chat/slash.lua).
2. Enable a real model through [Providers](providers-and-tools.md).
3. Understand [workflows and tools](workflows.md), then how
   [sessions and context](sessions.md) persist and compact work.
4. Read [Permissions](permissions.md) before relaxing safe mode.

For setup details, use [Installation](installation.md); for failures, start with
[Troubleshooting](troubleshooting.md). Read the
[starter reference](../README.md) before changing its composition,
or [Plugin authoring](../../../docs/plugin-authoring.md) to add a process plugin.
