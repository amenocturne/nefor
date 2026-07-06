# starter/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global `dispatch` hook, and spawns every actor via `actor.spawn` (sessions, agentic-loop, providers, mag kernel, tool-gate, lead-workflow, chat surface).
- `chat/` — chat surface **opinion layer** over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script` (it installs the require searchers and grafts the shared `lua/` tree onto the tui VM's `package.path`); the config-owned files are `statusline.lua`, `slash.lua`, and `update.lua`. The mechanism submodules (transcript, popups, history, sessions, at_path, view, entry, entries, run_panel, agent_streams, height_cache, common, log) now live in `lua/libs/chat/` and are pulled in as `require("libs.chat.<m>")`.
- the virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"` now ships in the shared Lua tree at `lua/libs/cli/` (require as `libs.cli`), not here.
- `agentic-loop/` — the config-owned turn-program (`lead-turn.mag`) plus a re-export shim. The spawner mechanism itself now ships in the shared Lua tree at `lua/libs/agentic-loop/` (require as `libs.agentic-loop`), not here. (The mag kernel likewise ships with the mag plugin at `plugins/mag/lua/mag-kernel/`.)
- `lead-workflow/` — re-export shims for the lead-workflow mechanism, which now lives in the shared lua tree at `lua/libs/lead-workflow/` (actor plan/approval state, kernel-run tracking, mag-eval tool, and the lead role's prompt loader). The persona prompt itself stays config-owned at `prompts/lead.md`.
- `sessions/` — a re-export shim; the session-management actor (boot / shutdown / resume + jsonl persistence over the bus) now ships in the shared Lua tree at `lua/libs/sessions/` (require as `libs.sessions`), and `require("sessions")` still resolves through the shim.
- actor-spec builders per Rust plugin binary (provider, tools) plus the chat-bridge wrapper that hosts `nefor-tui` now ship in the shared Lua tree at `lua/libs/compositors/` and are pulled in as `require("libs.compositors.<m>")`.
- `read-only-tools/` — a thin composition file. The mechanism (list_dir / search_text / python-read / instructions / discover_instruction_files handlers plus advertise/dispatch) ships in the shared Lua tree at `lua/libs/read-only-tools/`; this file declares the config's tool set by calling `require("libs.read-only-tools").build{ extra_tools = ... }` — starter builds the base set with no extras.
- `tool-validator/` — a thin composition file. The permission-classification mechanism (bash-via-`da`, edit/write policy, read-only handling) ships at `lua/libs/tool-validator/`; this file declares the config's policy via `require("libs.tool-validator").build{ auto_approve_tools = ..., bash_fastpaths = ... }` — starter builds the base policy with no extras.
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
