# starter/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global `dispatch` hook, and spawns every actor via `actor.spawn` (sessions, agentic-loop, providers, mag kernel, tool-gate, lead-workflow, chat surface).
- `chat/` — chat surface **opinion layer** over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script` (it installs the require searchers and grafts the shared `lua/` tree onto the tui VM's `package.path`); the config-owned files are `statusline.lua`, `slash.lua`, and `update.lua`. The mechanism submodules (transcript, popups, history, sessions, at_path, view, entry, entries, run_panel, agent_streams, height_cache, common, log) now live in `lua/libs/chat/` and are pulled in as `require("libs.chat.<m>")`.
- `cli/` — virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"`.
- `agentic-loop/` — the lead's turn spawner: per user message it clones the shipped turn-program (`agentic-loop/lead-turn.mag`) and submits it to the mag kernel. (The kernel itself now ships with the mag plugin at `plugins/mag/lua/mag-kernel/`, not here.)
- `lead-workflow/` — lead-workflow actor (plan/approval state, kernel-run tracking) plus the lead role's system prompt.
- `sessions/` — session-management actor: boot / shutdown / resume + jsonl persistence over the bus.
- `compositors/` — actor-spec builders per Rust plugin binary (provider, tools) plus the chat-bridge wrapper that hosts `nefor-tui`.
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

- **Add/remove plugins**: edit the `actor.spawn` blocks in `init.lua`.
- **Resume a prior session**: emit `sessions.resume_request { session_id = "<uuid>" }` on the bus (the chat slash-command surface does this for you).
- **Switch provider/model**: edit the `providers` list in `config/init.lua`. Both `mock-plugin` and `ollama` are spawned out of the box; pick a model interactively via `/model` in the TUI, or change `default_provider` / `default_model` to set the first-turn default.
