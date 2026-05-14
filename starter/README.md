# starter/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, installs the dispatch hook, spawns every plugin, wires the orchestration graph.
- `chat/` — chat surface composed over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script`; submodules under `chat/` carry per-concern code (transcript, statusline, input, popups, history, sessions, slash, at_path, view, update).
- `cli/` — virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"`.
- `agentic-loop/` — orchestrator state machine.
- `reasoners/` — Lua-resident reasoner type handlers (responder, terminal, tool-executor, adapter, provider-wrapper, agent, run, loop_counter).
- `lead-workflow/` — lead-workflow actor (plan/approval state, active graph run id) plus the lead role's system prompt and tool allowlist.
- `sessions/` — session-management actor: boot / shutdown / resume + jsonl persistence over the bus.
- `compositors/` — actor-spec builders per Rust plugin binary (provider, tools, graph, combinators) plus the chat-bridge wrapper that hosts `nefor-tui`.
- `mock-provider/` — script the `mock-plugin` binary loads to impersonate an openai-compatible provider with deterministic responses.
- `config/` — settings table (`require("config").active`) and binary-path resolver.
- `prompts/` — markdown system prompts referenced by Lua actors.

## Run

In-tree (debug build):

```sh
just run
```

Equivalent to:

```sh
cargo build --workspace
NEFOR_CONFIG_DIR=$PWD/starter NEFOR_PLUGIN_DIR=$PWD/target/debug \
  RUST_LOG=debug cargo run --bin nefor
```

Installed (after `brew install amenocturne/tap/nefor`):

```sh
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/starter/* ~/.config/nefor/
nefor
```

## Customize

- **Add/remove plugins**: edit the `ncp.spawn { ... }` blocks in `init.lua`.
- **Resume a prior session**: emit `sessions.resume_request { session_id = "<uuid>" }` on the bus (the chat slash-command surface does this for you). `sessions.lua` handles the rest in-process.
- **Change protocol behavior**: `ncp.lua` is where handshake, broadcast, replay, and error rules live.
- **Switch provider/model**: edit the `providers` list in `config.lua`. Both `mock-plugin` and `ollama` are spawned out of the box; pick a model interactively via `/model` in the TUI, or change `default_provider` / `default_model` to set the first-turn default.
