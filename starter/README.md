# starter/

Reference config for the nefor engine. NCP v0.1 protocol semantics live here in Lua — the Rust engine is a pure string-bus.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the `dispatch` hook (delegates to `ncp.dispatch`), spawns plugins via `nefor.plugins.spawn`, and wires the orchestration graph.
- `ncp.lua` — NCP v0.1 protocol module: handshake (`ready` / `ready_ok`), broadcast-minus-sender, replay-on-attach, error emission. JSON via `nefor.json` (serde_json through mlua).
- `agentic_workflow.lua` — orchestration glue: per-edge transform factories, reasoner-type handlers, `spawn_graph` tool, chat-input intake.
- `sessions.lua` — boot / shutdown / resume + jsonl persistence over the bus.
- `chat.lua` — chat surface as a Lua composition over `tui.*` primitives.
- `agentic_cli.lua` — virtual `agentic-cli` plugin for `nefor plugin agentic-cli "<prompt>"`.
- `ncp_test.lua`, `agentic_workflow_test.lua` — Lua unit tests, driven by `crates/nefor/tests/starter_*_test.rs`.

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
cp $(brew --prefix)/share/nefor/starter/*.lua ~/.config/nefor/
nefor
```

## Customize

- **Add/remove plugins**: edit the `ncp.spawn { ... }` blocks in `init.lua`.
- **Resume a prior session**: emit `sessions.resume_request { session_id = "<uuid>" }` on the bus (the chat slash-command surface does this for you). `sessions.lua` handles the rest in-process.
- **Change protocol behavior**: `ncp.lua` is where handshake, broadcast, replay, and error rules live.
- **Switch provider/model**: edit the `PROVIDER_NAME` / `PROVIDER_MODEL` block in `init.lua`. `PROVIDER_MODEL = nil` works but the chat surface won't be useful until you set one.
