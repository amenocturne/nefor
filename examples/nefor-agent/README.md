# examples/nefor-agent/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global `dispatch` hook, and spawns every actor via `actor.spawn` (sessions, conversation manager, agentic-loop, providers, mag kernel, read-only-tools, tool-validator, tool-gate, basic-tools, lead-workflow, chat surface).
- `chat/` — the canonical chat consumer over `tui.*` primitives. `chat/init.lua` is the entry point loaded by `nefor-tui --script`; transcript state is projected only from conversation-manager events. Configs extend commands and presentation through `config.active.chat_extension` rather than copying the reducer. Shared mechanisms live in `lua/libs/chat/`.
- the virtual `agentic-cli` plugin used by `cli-config/init.lua` for `nefor plugin agentic-cli "<prompt>"` now ships in the shared Lua tree at `lua/libs/cli/` (require as `libs.cli`), not here.
- `agentic-loop/` — the config-owned turn-program (`lead-turn.mag`) plus a re-export shim. The shared mechanism at `lua/libs/agentic-loop/` (require as `libs.agentic-loop`) owns transient turn submission, queueing, and ephemeral provider-chat orchestration. Canonical recorded conversation facts, context, and provider-neutral projections belong to `lua/libs/conversation-manager/`. (The MAG kernel likewise ships with the MAG plugin at `plugins/mag/lua/mag-kernel/`.)
- `lead-workflow/` — re-export shims for the lead-workflow mechanism, which now lives in the shared lua tree at `lua/libs/lead-workflow/` (actor plan/approval state, kernel-run tracking, MAG source/application tools, and prompt loading). The complete lead and worker system prompts stay config-owned at `prompts/lead.md` and `prompts/worker.md`; they own orchestration and cross-tool selection policy, while `libs.mag-context` supplies canonical MAG reference material and provider-native tool schemas supply individual signatures. The loader adds only Nefor's canonical bounded-context instruction.
- `sessions/` — a re-export shim; the session-management actor (boot / shutdown / resume + jsonl persistence over the bus) now ships in the shared Lua tree at `lua/libs/sessions/` (require as `libs.sessions`), and `require("sessions")` still resolves through the shim.
- actor-spec builders per Rust plugin binary (provider, tools) plus the chat-bridge wrapper that hosts `nefor-tui` now ship in the shared Lua tree at `lua/libs/compositors/` and are pulled in as `require("libs.compositors.<m>")`.
- `read-only-tools/` — a thin composition file selecting `discover_instruction_files` from `libs.read-only-tools`. The shared library also provides an opt-in ordinary `skill` loader and the `extra_tools` registration seam for configuration-owned capabilities. Repository discovery reports AGENTS.md/CLAUDE.md paths without loading their contents.
- `tool-validator/` — this configuration's classifier flags for each approval policy (`da`: the strict read-only rules for `ReadOnly`, the wide rules plus the `--ml` model for `Default`). The command-classification mechanism ships at `lua/libs/tool-validator/` and has no flags of its own.
- `mag/lib/nefor/toolsets.json` — the configuration's toolsets. Each states its approval policy together with its tools, `{"read_only": {"policy": "ReadOnly", "tools": [...]}, "general": {"policy": "Default", "tools": [...]}}`; a toolset without a policy, with an unknown policy, or with another key does not load. The canonical `nefor.toolsets` module in the registered `nefor-mag` package reads the manifest and builds typed `Toolset` values (`nefor.read_only_tools`, `nefor.general_tools`), and `libs.mag-context` renders the same file in the prompt. The tool gate's owner-qualified runtime advertisement is the schema authority: MAG projects each toolset's tools through that snapshot before provider dispatch, and the original names remain the invocation allowlist. An agent may also be given `Toolset {policy: ..., tools: [...]}` directly.
- `mag/lib/agents.mag` — the configuration's public MAG module: the model choices, `AgentConfig`, and `agent`, each with a description comment. `init.lua` lists it in `public_modules`, so its source is embedded in every MAG-authoring prompt together with `nefor.contracts` and `nefor.shell`; Nefor's other public modules are listed there with their paths only, and `toolsets` renders the manifest above. `mag/lib/agents/models.mag` holds the model-to-provider resolution and is not shown.
- `mag/lib/prompts/` — config-owned prompt fragments available as immutable file inputs. Canonical MAG modules and the MAG Book live once in the registered runtime package at `<runtime>/mag`; each workspace in a session contains only the source its node wrote. Reusable libraries are registered package roots rather than copied into sessions.
- the tool-gate wrapper in `lua/libs/compositors/tools.lua` records private `context.folders` metadata from tool advertisements, strips it before the public registry, and emits path-only, per-chat/scope de-duped `AGENTS.md` / `CLAUDE.md` reminders before forwarding tool invokes. The Rust gate owns policy enforcement; instruction-file reminder behavior lives in Lua composition, and file contents are not loaded automatically.
- `mock-provider/` — the interactive demo behavior the `mock-plugin` binary loads: canned triggers and help text built on [`libs.mock-provider`](../../lua/libs/mock-provider/init.lua), which supplies the provider wire protocol. `NEFOR_MOCK_PROVIDER_SCRIPT` selects a different behavior script, such as a test's own scenario.
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
NEFOR_DEV_DIR=$PWD NEFOR_CONFIG_DIR=$PWD/examples/nefor-agent \
  RUST_LOG=debug cargo run --bin nefor
```

Installed (after `brew install amenocturne/tap/nefor`):

```sh
mkdir -p ~/.config/nefor
cp -r $(brew --prefix)/share/nefor/examples/nefor-agent/* ~/.config/nefor/
nefor
```

## Runtime root contract

Installed distributions set `NEFOR_RUNTIME_ROOT` to their immutable, installer-managed Nefor checkout. The starter and chat runtime load Lua and plugin support only from that root (with plugin commands selected by the distribution helper). The starter registers those already-materialized module roots in-memory and does not create package-manager links or lockfiles. `NEFOR_DEV_DIR` is the sole live-checkout override and is intended for explicit in-repository development such as `just run`. Source-repository registry fields and filesystem proximity are never runtime roots.

## Customize

- **Add/remove plugins**: edit the `actor.spawn` blocks in `init.lua`.
- **Extend chat commands/presentation**: launch `nefor-tui --script <nefor-root>/examples/nefor-agent/chat/init.lua`, set `chat_extension = "my-chat-extension"` in the foreign config's `config.active`, and return a module with optional `commands`, `initial_state`, `status_segments`, and `input_border_style` fields. `nefor-tui` binds `chat.commands`, `chat.slash`, and `chat.statusline` to that canonical script directory; `$NEFOR_CONFIG_DIR/chat` cannot shadow them. Command handlers receive `(args, read_only_state, api)` and either return `nil` to defer to the canonical command or return `next_state, effects`. Use `api.finish(patch)` for an ordinary completed command, `api.patch(patch)` when preserving input chrome, and `api.new_session(patch)` for extension-defined switches that need the canonical session reset. A command sharing a canonical name must set `extend = true`; its argument completions are merged while its handler may selectively defer. Extensions never receive mutable canonical state and must not reduce provider or conversation events.
- **Resume a prior session**: emit `sessions.resume_request { session_id = "<uuid>" }` on the bus (the chat slash-command surface does this for you).
- **Switch provider/model**: edit the `providers` list in `config/init.lua`. `mock-plugin` is spawned out of the box and is the default provider for deterministic startup. `chatgpt` and `ollama` are opt-in via `NEFOR_ENABLE_CHATGPT=1` and `NEFOR_ENABLE_OLLAMA=1` (or by editing `config/init.lua`). Pick a model interactively via `/model` in the TUI, or change `default_provider` / `default_model` to set the first-turn default.
- **Compact ChatGPT context**: `/compact` asks ChatGPT’s native Responses compaction endpoint to seal the conversation so far. Later turns restore that compacted context into their fresh provider chats; switching provider or model falls back to the full local transcript.
- **Inspect provider usage**: `/usage` queries the configured exact usage IDs through conversation-manager. ChatGPT contributes live subscription windows; optional OpenRouter contributes a session-local USD total only after authoritative non-BYOK cost evidence. The footer renders each present value independently.

## Example guides

The composition and `/help` are authoritative for commands and keys. Supporting
workflows that are not evident from the declarative Lua live here:

- [Getting started](docs/getting-started.md) and [installation](docs/installation.md)
- [Customization](docs/customization.md), [providers and tools](docs/providers-and-tools.md), and [distribution](docs/distribution.md)
- [Permissions](docs/permissions.md), [sessions](docs/sessions.md), and [workflows](docs/workflows.md)
- [Chat extension seam](docs/chat-extensions.md) and [troubleshooting](docs/troubleshooting.md)

Provider wire/API details belong to each provider plugin README; generic TUI
capabilities belong to [`plugins/nefor-tui`](../../plugins/nefor-tui/README.md).
