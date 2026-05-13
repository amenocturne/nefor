# nefor — architecture map

## What this is

Agent harness substrate. Pure string-bus engine + separate-process plugins (NCP v0.1 over JSON-line stdio) + Lua composition. Plugins can be Rust or any language that can produce JSON lines on stdout and consume them on stdin. Lua stays embedded for `init.lua` composition; the rest is process-isolated.

## Layout

- `crates/nefor-combinators/` — in-process algebra library (pure Rust, minimal deps). Trait shapes for Rust-native plugins. The canonical combinator library at runtime is the plugin, not the crate.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types. Used by plugins; engine no longer imports it (engine is pure string-bus).
- `crates/nefor/` — engine binary. Reads plugin stdin, stamps `{origin, ts}`, persists to session log, invokes a required Lua `dispatch` hook, routes the hook's `nefor.engine.send` calls. All NCP semantics live in Lua.
- `plugins/nefor-tui/` — declarative TUI plugin (Rust): reconciler + line-diff renderer + Lua VM + 15 layout primitives. Hosts the chat surface as a Lua composition (`starter/chat/init.lua`).
- `plugins/nefor-combinators/` — typed combinator registry keyed by `Identity (arity, input_type, output_multiset)`; per-trait constraint validation (Merge, Into, Fanout, Equivalent).
- `plugins/generic-provider/`, `plugins/generic-tool/` — passive type-registry hubs owning canonical types (`ProviderIn`, `ProviderOut`, `ChatHistory`, `ToolCalls`, `ToolResults`, …). Concrete providers/tools declare `Into`/`From` against these so graphs are provider-agnostic.
- `plugins/openai-provider/` — generic OpenAI-compatible provider with chat-id-keyed `Chats` map (`<prefix>.chat.{create, append, complete, delete}`). Configurable base URL + model. Declares `Into` against `generic-provider` types.
- `plugins/reasoner-graph/` — typed graph scheduler. Cycles allowed. Per-firing lifecycle, `prev_state`/`next_state` carry, fanout-based type-dispatched routing, ack/result lifecycle, broadcast `dag.run_started` / `dag.node_dispatched` for UI observability.
- `plugins/tool-gate/` — tool advertisement aggregator + permission gate. Sources advertise via `tools.advertise`; callers invoke via `tool.invoke`; gate forwards as `<source>.tool.invoke` and echoes `tool.result`.
- `plugins/basic-tools/` — `read_file` / `write_file` / `bash` built-ins.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests. Local Ollama works through `openai-provider` directly with `static_token = "ollama-local"`.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `starter/init.lua` — default composition. Sets `package.path`, defines the global `dispatch` hook (delegates to `core.ncp.dispatch`), spawns plugins via `nefor.plugins.spawn`, wires per-edge `from_plugin`/`to_plugin` transforms.
- `lua/core/` — shipped library: NCP v0.1 (handshake, broadcast-minus-sender, replay-on-attach, errors), actor runtime, history replay. JSON via the engine-provided `nefor.json`.
- `starter/agentic-loop/` — orchestrator state machine.
- `starter/reasoners/` — Lua-resident reasoner type handlers (`responder`, `provider-wrapper`, `tool-executor`, `adapter`, `terminal`, `agent`, `run`, `loop_counter`).
- `starter/sessions/` — sessions actor: boot/shutdown/resume + jsonl persistence over the bus.
- `starter/chat/` — chat surface composed over `tui.*` primitives (entry `chat/init.lua`; transcript, statusline, input, popups, slash commands as submodules).
- `starter/cli/` — virtual `agentic-cli` plugin: surfaces the loop over stdin/stdout for `nefor plugin agentic-cli "<prompt>"`.
- `starter/lead-workflow/` — lead role plus the dispatch-graph / write-review / await-approval tool surface.
- `starter/compositors/` — actor-spec builders per plugin binary (provider, tools, graph, combinators, chat_bridge).
- `starter/mock-provider/` — script loaded by `mock-plugin` to impersonate an openai-compatible provider with deterministic responses.
- `starter/config/` — settings table consumed by `starter/init.lua`.

## Path resolution

`nefor` resolves directories via XDG-style env vars, with CLI flags taking highest precedence:

| Env var            | CLI flag          | Default                   | Holds       |
|--------------------|-------------------|---------------------------|-------------|
| `NEFOR_CONFIG_DIR` | `--config`        | `$XDG_CONFIG_HOME/nefor`  | `init.lua`  |
| `NEFOR_DATA_DIR`   | `--data-dir`      | `$XDG_DATA_HOME/nefor`    | sessions    |
| `NEFOR_PLUGIN_DIR` | `--plugin-dir`    | `$NEFOR_DATA_DIR/plugins` | binaries    |

If no `init.lua` is found, the engine prints a friendly error pointing at the README install section.

## Conventions (enforced)

- Errors: `thiserror` for domain errors, `anyhow` only at the top boundary (`main.rs`).
- No `unwrap()` / `expect()` outside tests.
- Newtype every domain ID (`PluginId`, `SessionId`, `RunId`, `NodeId`, `FiringId`, `ChatId`, `ConfigDir`, `DataDir`).
- Enums (ADTs) for state; no boolean flags alongside sentinel variants.
- Immutability by default; I/O only at boundaries.
- No YAML/TOML/JSON config schema in core — config is `init.lua`.
- Plugins are separate OS processes communicating via NCP (see `protocol/v0.1/spec.md`).
- Comments only for non-obvious *why*; code is self-documenting for *what*.

## Commands

- `just run` — launch engine with `./starter` config (debug build).
- `just test` — workspace tests.
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt.
- `just build` — release build into `target/release/`.

## Spec

- NCP wire spec: `protocol/v0.1/spec.md`.
- Architecture/writing principles: `docs/principles.md`.
