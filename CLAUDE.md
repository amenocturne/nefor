# nefor — architecture map

## What this is

Agent harness substrate. Pure string-bus engine + separate-process plugins (NCP v0.1 over JSON-line stdio) + Lua composition. Plugins can be Rust or any language that can produce JSON lines on stdout and consume them on stdin. Lua stays embedded for `init.lua` composition; the rest is process-isolated.

## Layout

- `crates/nefor-combinators/` — in-process algebra library (pure Rust, minimal deps). Trait shapes for Rust-native plugins. The canonical combinator library at runtime is the plugin, not the crate.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types. Used by plugins; engine no longer imports it (engine is pure string-bus).
- `crates/nefor/` — engine binary. Reads plugin stdin, stamps `{origin, ts}`, persists to session log, invokes a required Lua `dispatch` hook, routes the hook's `nefor.engine.send` calls. All NCP semantics live in Lua.
- `plugins/nefor-tui/` — declarative TUI plugin (Rust): reconciler + line-diff renderer + Lua VM + 15 layout primitives. Hosts the chat surface as a Lua composition (`starter/chat.lua`).
- `plugins/nefor-combinators/` — typed combinator registry keyed by `Identity (arity, input_type, output_multiset)`; per-trait constraint validation (Merge, Into, Fanout, Equivalent).
- `plugins/generic-provider/`, `plugins/generic-tool/` — passive type-registry hubs owning canonical types (`ProviderIn`, `ProviderOut`, `ChatHistory`, `ToolCalls`, `ToolResults`, …). Concrete providers/tools declare `Into`/`From` against these so graphs are provider-agnostic.
- `plugins/openai-provider/` — generic OpenAI-compatible provider with chat-id-keyed `Chats` map (`<prefix>.chat.{create, append, complete, delete}`). Configurable base URL + model. Declares `Into` against `generic-provider` types.
- `plugins/reasoner-graph/` — typed graph scheduler. Cycles allowed. Per-firing lifecycle, `prev_state`/`next_state` carry, fanout-based type-dispatched routing, ack/result lifecycle, broadcast `dag.run_started` / `dag.node_dispatched` for UI observability.
- `plugins/tool-gate/` — tool advertisement aggregator + permission gate. Sources advertise via `tools.advertise`; callers invoke via `tool.invoke`; gate forwards as `<source>.tool.invoke` and echoes `tool.result`.
- `plugins/basic-tools/` — `read_file` / `write_file` / `bash` built-ins.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests. Local Ollama works through `openai-provider` directly with `static_token = "ollama-local"`.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `starter/init.lua` — default composition. Sets `package.path`, defines the global `dispatch` hook (delegates to `ncp.dispatch`), spawns plugins via `nefor.plugins.spawn`, wires per-edge `from_plugin`/`to_plugin` transforms.
- `starter/ncp.lua` — NCP v0.1 in Lua (handshake, broadcast-minus-sender, replay-on-attach, errors). JSON via the engine-provided `nefor.json` (serde_json bridged through mlua).
- `starter/agentic_workflow.lua` — orchestration glue: per-edge transform factories (`for_provider`, `for_reasoner_graph`, `for_tool_gate`, `for_chat`), reasoner-type handlers (`responder`, `provider-wrapper`, `tool-executor`, `adapter`, `terminal`), `spawn_graph` tool binding, chat-input intake.
- `starter/sessions.lua` — Lua-side session library: boot/shutdown/resume + jsonl persistence over the bus.
- `starter/chat.lua` — chat surface as a Lua composition over `tui.*` primitives (transcript, statusline, input, popups, slash commands).
- `starter/agentic_cli.lua` — virtual `agentic-cli` plugin: surfaces `agentic_workflow` over stdin/stdout for `nefor plugin agentic-cli "<prompt>"`.

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
