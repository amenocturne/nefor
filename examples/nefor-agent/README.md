# examples/nefor-agent/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global `dispatch` hook, and spawns every actor via `actor.spawn` (sessions, conversation manager, agentic-loop, providers, mag kernel, read-only-tools, tool-validator, tool-gate, basic-tools, lead-workflow, chat surface).
- `chat/` — the canonical chat consumer over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script`; transcript state is projected only from conversation-manager events. Configs extend commands and presentation through `config.active.chat_extension` rather than copying the reducer. Shared mechanisms live in `lua/libs/chat/`.
- the virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"` now ships in the shared Lua tree at `lua/libs/cli/` (require as `libs.cli`), not here.
- `agentic-loop/` — the config-owned turn-program (`lead-turn.mag`) plus a re-export shim. The shared mechanism at `lua/libs/agentic-loop/` (require as `libs.agentic-loop`) owns transient turn submission, queueing, and ephemeral provider-chat orchestration. Canonical recorded conversation facts, context, and provider-neutral projections belong to `lua/libs/conversation-manager/`. (The MAG kernel likewise ships with the MAG plugin at `plugins/mag/lua/mag-kernel/`.)
- `lead-workflow/` — re-export shims for the lead-workflow mechanism, which now lives in the shared lua tree at `lua/libs/lead-workflow/` (actor plan/approval state, kernel-run tracking, mag-eval tool, and prompt composition). The complete lead and worker system prompts stay config-owned at `prompts/lead.md` and `prompts/worker.md` — the loader reads them verbatim (no runtime composition), so any provider preamble (e.g. reasoning-hygiene for local Ollama models) is folded into those files directly.
- `sessions/` — a re-export shim; the session-management actor (boot / shutdown / resume + jsonl persistence over the bus) now ships in the shared Lua tree at `lua/libs/sessions/` (require as `libs.sessions`), and `require("sessions")` still resolves through the shim.
- actor-spec builders per Rust plugin binary (provider, tools) plus the chat-bridge wrapper that hosts `nefor-tui` now ship in the shared Lua tree at `lua/libs/compositors/` and are pulled in as `require("libs.compositors.<m>")`.
- `read-only-tools/` — a thin composition file. The mechanism (list_dir / search_text / python-read / instructions / discover_instruction_files handlers plus advertise/dispatch) ships in the shared Lua tree at `lua/libs/read-only-tools/`; this file declares the config's tool set by calling `require("libs.read-only-tools").build{ extra_tools = ... }` — starter builds the base set with no extras. `python-read` is advertised but currently returns an MVP-unavailable error rather than running sandboxed Python analysis.
- `tool-validator/` — a thin composition file. The permission-classification mechanism (`shell.script`-via-`da` plus structural `process.exec`, edit/write policy, allowlist-derived read-only handling) ships at `lua/libs/tool-validator/`; this file parses the canonical `mag/lib/nefor/toolsets.json` manifest and supplies its read-only inventory to the mechanism.
- `mag/lib/nefor/toolsets.json` — config-owned requested profiles for common delegated-agent capability surfaces. `mag/lib/nefor/toolsets.mag` exposes them as `nefor.actors.read-only-tools` and `nefor.actors.general-tools` through MAG's typed JSON parser, while tool-validator uses the engine's JSON binding. The tool gate's owner-qualified runtime advertisement is the schema authority: MAG projects each requested profile through that snapshot before provider dispatch, and the original `:tools` names remain the invocation allowlist. Explicit custom lists are supported.
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
NEFOR_CONFIG_DIR=/examples/nefor-agent NEFOR_PLUGIN_DIR=$PWD/target/debug \
  RUST_LOG=debug cargo run --bin nefor
```

Installed (after `brew install amenocturne/tap/nefor`):

```sh
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/examples/nefor-agent/* ~/.config/nefor/
nefor
```

## Runtime root contract

Installed distributions set `NEFOR_RUNTIME_ROOT` to their immutable, installer-managed Nefor checkout. The starter and chat runtime load Lua and plugin support only from that root (with copied binaries selected separately through `NEFOR_PLUGIN_DIR`). The starter registers those already-materialized module roots in-memory and does not create package-manager links or lockfiles. `NEFOR_DEV_DIR` is the sole live-checkout override and is intended for explicit in-repository development such as `just run`. Source-repository registry fields and filesystem proximity are never runtime roots.

## Customize

- **Add/remove plugins**: edit the `actor.spawn` blocks in `init.lua`.
- **Extend chat commands/presentation**: launch `nefor-tui --script <nefor-root>/examples/nefor-agent/chat/init.lua`, set `chat_extension = "my-chat-extension"` in the foreign config's `config.active`, and return a module with optional `commands`, `initial_state`, `status_segments`, and `input_border_style` fields. `nefor-tui` binds `chat.commands`, `chat.slash`, and `chat.statusline` to that canonical script directory; `$NEFOR_CONFIG_DIR/chat` cannot shadow them. Command handlers receive `(args, read_only_state, api)` and either return `nil` to defer to the canonical command or return `next_state, effects`. Use `api.finish(patch)` for an ordinary completed command, `api.patch(patch)` when preserving input chrome, and `api.new_session(patch)` for extension-defined switches that need the canonical session reset. A command sharing a canonical name must set `extend = true`; its argument completions are merged while its handler may selectively defer. Extensions never receive mutable canonical state and must not reduce provider or conversation events.
- **Resume a prior session**: emit `sessions.resume_request { session_id = "<uuid>" }` on the bus (the chat slash-command surface does this for you).
- **Switch provider/model**: edit the `providers` list in `config/init.lua`. `mock-plugin` is spawned out of the box and is the default provider for deterministic startup. `chatgpt` and `ollama` are opt-in via `NEFOR_ENABLE_CHATGPT=1` and `NEFOR_ENABLE_OLLAMA=1` (or by editing `config/init.lua`). Pick a model interactively via `/model` in the TUI, or change `default_provider` / `default_model` to set the first-turn default.
- **Compact ChatGPT context**: `/compact` asks ChatGPT’s native Responses compaction endpoint to seal the conversation so far. Later turns restore that compacted context into their fresh provider chats; switching provider or model falls back to the full local transcript.
- **Inspect ChatGPT quota**: `/usage` shows both quota windows and reset times. When the active provider advertises usage support, the footer keeps a compact available-capacity gauge such as `◔ 34% until 14:30`.

## Example guides

The composition and `/help` are authoritative for commands and keys. Supporting
workflows that are not evident from the declarative Lua live here:

- [Getting started](docs/getting-started.md) and [installation](docs/installation.md)
- [Customization](docs/customization.md), [providers and tools](docs/providers-and-tools.md), and [distribution](docs/distribution.md)
- [Permissions](docs/permissions.md), [sessions](docs/sessions.md), and [workflows](docs/workflows.md)
- [Chat extension seam](docs/chat-extensions.md) and [troubleshooting](docs/troubleshooting.md)

Provider wire/API details belong to each provider plugin README; generic TUI
capabilities belong to [`plugins/nefor-tui`](../../plugins/nefor-tui/README.md).
