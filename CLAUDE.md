# nefor — AI context

## What this is
Rust rewrite of nefor: orchestration substrate for AI agents. Monorepo: pure algebra library + NCP-speaking engine + separate-process plugins (Rust or Lua). Terminal frontend, providers, scheduler, tools all ship as plugins. Lua glue in `starter/` wires them together; the engine is a pure string-bus.

## Layout
- `crates/nefor-combinators/` — in-process algebra library (pure Rust, minimal deps). Trait shapes for Rust-native plugins. The *canonical* combinator library at runtime is the plugin, not the crate.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types. Used by plugins; engine stopped importing it in Slice 2.
- `crates/nefor/` — engine binary. Pure string-layer event bus: reads plugin stdin, stamps `{origin, ts}`, persists to session log, invokes a required Lua `step` hook, routes step's `nefor.engine.send` calls. All NCP semantics live in Lua.
- `plugins/nefor-tui/` — Rust NCP plugin: ratatui/crossterm terminal frontend with text selection + clipboard.
- `plugins/nefor-chat/` — Rust NCP plugin: chat UI consuming `chat-contract v0.1` (statusline v2, markdown rendering, tool one-liners, DAG running-nodes sidebar pane behind Ctrl-B).
- `plugins/mock-plugin/` — Rust NCP plugin wrapping the `claude` CLI; emits `cc.*` events; declares `Context`/`Message` types + `Merge<Message>` handler. Adapter `starter/mock_plugin_adapter.lua` translates `cc.*` ↔ chat-contract.
- `plugins/nefor-combinators/` — Rust NCP plugin: typed combinator registry keyed by `Identity (arity, input_type, output_multiset)`; per-trait constraint validation (Merge, Into, Fanout, Equivalent).
- `plugins/generic-provider/`, `plugins/generic-tool/` — passive type-registry hubs owning canonical types (`ProviderIn`, `ProviderOut`, `ChatHistory`, `ToolCalls`, `ToolResults`, …). Concrete providers/tools declare `Into`/`From` against these so graphs are provider-agnostic.
- `plugins/openai-provider/` — generic OpenAI-compatible provider with chat-id-keyed `Chats` map (`<prefix>.chat.{create, append, complete, delete}`). Configurable base URL + model. Declares `Into` against `generic-provider` types.
- `plugins/reasoner-graph/` — typed graph scheduler (renamed from `dag-scheduler`). Cycles allowed. Per-firing lifecycle (`firings: HashMap<NodeId, Vec<NodeFiring>>`), `prev_state`/`next_state` carry, fanout-based type-dispatched routing, ack/result lifecycle, broadcast `dag.run_started` / `dag.node_dispatched` for UI observability.
- `plugins/tool-gate/` — tool advertisement aggregator + permission gate. Sources advertise via `tools.advertise`; callers invoke via `tool.invoke`; gate forwards as `<source>.tool.invoke` and echoes `tool.result`.
- `plugins/basic-tools/` — read_file/write_file/bash built-ins.
- `plugins/ollama/` — placeholder; auth shim. The actual auth is `static_token = "ollama-local"` against openai-provider's auth gate.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `starter/init.lua` — glue: package.path, `step` delegator, plugin spawn order, per-edge `from_plugin`/`to_plugin` transforms.
- `starter/ncp.lua` — NCP v0.1 in Lua (handshake, broadcast, replay, errors). JSON via `nefor.json` (serde_json bridged through mlua, ~10–40× faster than rxi/json.lua).
- `starter/reasoner_graph_adapter.lua` — type-driven adapter for the scheduler. Handles `<reasoner>.run_node` for `dummy`, `responder`, `provider-wrapper`, `tool-executor`, `adapter`, `terminal`. Drives openai-provider + tool-gate; emits ack and `graph.node_result`.
- `starter/spawn_graph.lua` — Lua tool binding exposing `spawn_graph` in the orchestrator's catalog. Translates tool-invoke → `reasoner-graph.run`, run-complete → `tool.result`.
- `starter/chat_orchestrator.lua` — `chat.input.submit` ↔ orchestrator template graph. Persists `next_state` (chat_id) across submits via in-process `on_node_result` observer hook.
- `starter/openai_provider_adapter.lua` — provider-side chat-contract bridge.

## Conventions (enforced)
- Errors: `thiserror` for domain errors, `anyhow` only at the top boundary (`main.rs`).
- No `unwrap()` / `expect()` outside tests + exhausted-paths in `main.rs`.
- Newtype every domain ID (`PluginId`, `SessionId`, `RunId`, `NodeId`, `FiringId`, `ChatId`).
- Enums (ADTs) for state; no boolean flags alongside sentinel variants.
- Immutability by default; I/O only at boundaries.
- No YAML/TOML/JSON config schema in core — config is `init.lua`.
- Plugins are separate OS processes communicating via NCP (see `protocol/v0.1/spec.md`). Any language. Lua stays embedded for `init.lua` composition.
- Comments only for non-obvious *why*; code is self-documenting for *what*.

## Commands
- `just run` — launch engine with `./starter` config (debug build).
- `just test` — workspace tests (~1,300 passing). `cargo test -p nefor --test stage1_e2e -- --ignored` for the live-Ollama smoke test.
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt + prettier on markdown.
- `just build` — release build into `target/release/`.

## Spec
Source of truth lives in the vault, not here:
- NCP wire spec: `protocol/v0.1/spec.md` (in this repo).
- Architecture/writing principles: `docs/principles.md` (in this repo).

## Milestone status

**Stage 1 shipped** (reasoner-everywhere foundations). 14 commits, latest `1ddadda` on `origin/main`. Reasoner-graph plugin runs typed graphs with cycles + per-firing lifecycle + fanout combinators; `generic-provider`/`generic-tool` own canonical type tags; `openai-provider` keyed by chat-id; `tool-gate` + `basic-tools` cover tool advertisement and permission flow; `spawn_graph` Lua tool exposes sub-graph submission; `chat_orchestrator` + `reasoner_graph_adapter` wire chat → orchestrator template (`provider-wrapper` + `tool-executor` + `adapter` + `terminal` cycle). End-to-end chat works in TUI: streaming, history persistence across submits via in-process observer hook, `/new` clears state, system prompt + tool catalog coexist on gemma4. Validated by `crates/nefor/tests/stage1_e2e.rs` (`#[ignore]`-gated; needs live Ollama).

**Slice 3 shipped** (TUI polish + chat-contract). nefor-chat consumes vendor-neutral `chat.*` events; mock-plugin stays producer-clean (`cc.*`); per-edge transforms in `ncp.spawn { from_plugin, to_plugin }` are the bridge. Statusline v2 (model · context-bar · cost · turns · last-turn duration), pulldown-cmark markdown, one-liner tool I/O.

**Slice 2 shipped** (engine = pure event bus + sessions). Engine reads stdin lines, stamps `{origin, ts}`, persists to `$XDG_DATA_HOME/nefor/sessions/<id>.jsonl`, invokes `step(saved_log, current_log)`. NCP v0.1 implemented entirely in `starter/ncp.lua`. `nefor.parent_session` loads a prior log into `saved_log` for resume / impersonation.

**Slice 1 shipped** (combinator foundations). `nefor-combinators` plugin = type-aware registry + dispatch executor; mock-plugin declares `Context`/`Message` + `Merge<Message>` handler. Validated by `combinators_slice1.rs`.

**M2 shipped** (Claude on screen). Engine spawns `mock-plugin + nefor-chat + nefor-tui` as three processes; prompts flow user → tui → chat → harness → Claude and responses stream back. `/resume` reloads prior session history.

**Recent UI polish.** DAG running-nodes panel above the statusline driven by `dag.run_started` / `dag.node_dispatched` lifecycle events; right sidebar pane (Ctrl-B toggle, persistent, vertical separator); `/dag-test` slash command for end-to-end DAG smoke testing; colored DAG widget rows; finished tasks linger 2s before pruning.

## Open work (priority order)

### 1. Reasoning-stream UI (in flight, Session 3 subagent)

`delta.reasoning` is currently ignored entirely. A reasoning-stream + collapse UI is being built that streams reasoning live, collapses to `[reasoned]` when content starts (mirrors tool-call collapse), expands via the same key combo as tool I/O details, and keeps reasoning OUT of stored assistant content / next-turn history. New event kinds: `<prefix>.stream.reasoning_delta`, `<prefix>.stream.reasoning_end` (provider native), translated to `chat.stream.reasoning_delta` / `chat.stream.reasoning_end` (chat-contract).

### 2. Stage 1 smoke target — DONE this session


### 3. UX polish carry-overs from Session 3

- **Sidebar count "1/4 most of the time"** — `handle_dag_node_dispatched` overwrites the entry on cycle re-dispatch, resetting Done→Running. Better display: "X done / Y dispatched" (no reset) or per-firing rows. `nefor-chat/render.rs#run_id_prefix_spans` + `nefor-chat/main.rs#handle_dag_node_dispatched`.
- **No UI signal that a sub-graph spawn is in flight.** With async `spawn_graph` the chat unblocks fast; users have no badge telling them "background work running." Hook `spawn_graph.completed` events to a status-line indicator or a `chat.popup`.
- **Permission-prompt fatigue.** `tool-gate --default prompt` prompts every spawn. `--allow spawn_graph` for trusted tools, or sticky-allow per session, would smooth iteration.
- **Timeout / cancellation for deferred completions.** `chat_orchestrator.attach_spawn_graph_listener` queues forever; a runaway sub-graph never times out. Out-of-scope for v1; capture for Stage 2.

### 4. Stage 2 scoping

- **Multi-session resume.** `current_state` (chat_id) and `deferred_queue` are Lua-resident — die with the engine. Spec §8 deferred this for Stage 1; revisit. Persist deferred queue too so engine restart mid-spawn doesn't silently lose results.
- **`initial_ctx` graph parameter** instead of the `args.seed_chat_id` hack chat_orchestrator currently uses (parent spec §7 open question #2).
- **Concurrent submit handling.** Currently second-submit-while-busy is dropped with a system message; with async spawn the chat is "busy" much less but multi-in-flight tracking is needed.
- **Tool-gate policy refinements.** Per-tool / per-plugin policies, sticky-allow, audit log.
- **`graph.cancel` UI hook.** Wire kind exists; chat `/new` mid-run could fire it.

### 5. Cleanup loose ends

- `plugins/ollama/` is empty; decide between real auth/login flow vs placeholder.
- Verbose pipeline logging (especially `openai_provider::http` body-dump) is great for diagnostics, noisy at steady state. Already gated behind `RUST_LOG=info`; consider trimming once trusted.
- `stage1_e2e.rs` is `#[ignore]`-gated for live Ollama. With the mock provider in place, write a non-ignored e2e using `USE_MOCK_PROVIDER=true` so CI can validate the full async pipeline without an LLM.
- Mark Stage 1 deliverables done in `nefor-agent-and-reasoner-types-spec.md`.
- **Reasoning-only-model opt-in flag** (post-reasoning-UI). For models like gemma4 that emit only `delta.reasoning` and no `delta.content`, add an `--include-reasoning-as-content` provider flag (or per-chat setting). Default off; opt-in only.

### Pre-existing, still valid
- **mock-plugin-as-Reasoner.** Add id-correlated `cc.run { id, context } → cc.run.result { id, message }` alongside the broadcast `cc.prompt` flow. Needed once orchestrator graphs want mock-plugin as a node.
- **Session resumption semantics** (D-21a-deferred). Slice 2 ships the substrate; the next layer is per-plugin declarative replay protocol.
- **Graceful shutdown** from starter via `nefor.engine.on_shutdown(fn)`.
- **Leaf Reasoners** — `at-file`, `review-terminal`, `review-file-annotation`.
- **Plugin-root resolver polish** — engine prefers XDG even when the dir doesn't exist.
- **Syntax highlighting** inside markdown code blocks in nefor-chat.

Deferred / not coming: WASM runtime, persona system, hook runner, plugin manager (Mason-style), capability model, multi-frontend (GUI/web/telegram beyond the NCP-first terminal) — all post-MVP plugin-land.
