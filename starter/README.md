# starter/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global `dispatch` hook, and spawns every actor via `actor.spawn` (sessions, state-tracking, agentic-loop, providers, mag kernel, read-only-tools, tool-validator, tool-gate, basic-tools, lead-workflow, chat surface).
- `chat/` — chat surface **opinion layer** over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script` (it installs the require searchers and grafts the shared `lua/` tree onto the tui VM's `package.path`); the config-owned files are `statusline.lua`, `slash.lua`, and `update.lua`. The mechanism submodules (transcript, popups, history, sessions, at_path, view, entry, entries, run_panel, agent_streams, height_cache, common, log) now live in `lua/libs/chat/` and are pulled in as `require("libs.chat.<m>")`.
- the virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"` now ships in the shared Lua tree at `lua/libs/cli/` (require as `libs.cli`), not here.
- `agentic-loop/` — the config-owned turn-program (`lead-turn.mag`) plus a re-export shim. The spawner mechanism itself now ships in the shared Lua tree at `lua/libs/agentic-loop/` (require as `libs.agentic-loop`), not here. (The mag kernel likewise ships with the mag plugin at `plugins/mag/lua/mag-kernel/`.)
- `lead-workflow/` — re-export shims for the lead-workflow mechanism, which now lives in the shared lua tree at `lua/libs/lead-workflow/` (actor plan/approval state, kernel-run tracking, mag-eval tool, and the lead role's prompt loader). The persona prompt itself stays config-owned at `prompts/lead.md`.
- `sessions/` — a re-export shim; the session-management actor (boot / shutdown / resume + jsonl persistence over the bus) now ships in the shared Lua tree at `lua/libs/sessions/` (require as `libs.sessions`), and `require("sessions")` still resolves through the shim.
- actor-spec builders per Rust plugin binary (provider, tools) plus the chat-bridge wrapper that hosts `nefor-tui` now ship in the shared Lua tree at `lua/libs/compositors/` and are pulled in as `require("libs.compositors.<m>")`.
- `read-only-tools/` — a thin composition file. The mechanism (list_dir / search_text / python-read / instructions / discover_instruction_files handlers plus advertise/dispatch) ships in the shared Lua tree at `lua/libs/read-only-tools/`; this file declares the config's tool set by calling `require("libs.read-only-tools").build{ extra_tools = ... }` — starter builds the base set with no extras. `python-read` is advertised but currently returns an MVP-unavailable error rather than running sandboxed Python analysis.
- `tool-validator/` — a thin composition file. The permission-classification mechanism (bash-via-`da`, edit/write policy, read-only handling) ships at `lua/libs/tool-validator/`; this file declares the config's policy via `require("libs.tool-validator").build{ auto_approve_tools = ..., bash_fastpaths = ... }` — starter builds the base policy with no extras.
- the tool-gate wrapper in `lua/libs/compositors/tools.lua` records private `context.folders` metadata from tool advertisements, strips it before the public registry, and emits path-only, per-chat/scope de-duped `AGENTS.md` / `CLAUDE.md` reminders before forwarding tool invokes. The Rust gate owns policy enforcement; instruction-file reminder behavior lives in Lua composition, and file contents are not loaded automatically.
- `mock-provider/` — script the `mock-plugin` binary loads to impersonate an openai-compatible provider with deterministic responses.
- `config/` — settings table (`require("config").active`) and binary-path resolver.
- `prompts/` — markdown system prompts referenced by Lua actors.

## Run

Interactive startup arguments are composition-owned and may appear in any order:

```sh
nefor run [--session <id>] [--prompt <text>] [--mode safe|auto|yolo]
nefor run --yolo [--session <id>] [--prompt <text>]
```

`--yolo` is shorthand for `--mode yolo`. If mode controls repeat, the last one wins. Resumed sessions start in safe mode unless a mode is supplied explicitly on this invocation.

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
- **Switch provider/model**: edit the `providers` list in `config/init.lua`. `mock-plugin` is spawned out of the box and is the default provider for deterministic startup. `chatgpt` and `ollama` are opt-in via `NEFOR_ENABLE_CHATGPT=1` and `NEFOR_ENABLE_OLLAMA=1` (or by editing `config/init.lua`). Pick a model interactively via `/model` in the TUI, or change `default_provider` / `default_model` to set the first-turn default.
- **Inspect ChatGPT quota**: `/usage` shows both quota windows and reset times. When the active provider advertises usage support, the footer keeps a compact available-capacity gauge such as `◔ 34% until 14:30`.
