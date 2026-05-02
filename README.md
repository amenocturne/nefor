# nefor

> do whatever you want.

Rust rewrite of the nefor agent harness. NCP-speaking engine + ratatui terminal frontend + Claude-Code wrapper, all as separate processes.

**Status: M2 shipped.** End-to-end Claude chat in a TUI, composed from three plugins spoken over NCP v0.1. Plugin management, permission-gate, and DAG orchestration are post-MVP.


## Quick start

```sh
just setup   # cargo fetch
just run     # launch engine + default config
just test    # workspace tests (hermetic)
```

Manual smoke against the local Ollama provider (default starter config):

```sh
cargo build --workspace
NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter
```

Ctrl+C (or `/quit` in chat) exits cleanly; `/new` clears the transcript and starts a new chat.

## Layout

- `crates/nefor-combinators/` — pure Rust substrate (Context, Reasoner, combinators).
- `crates/nefor-protocol/` — NCP v0.1 types + parsers.
- `crates/nefor/` — engine binary: NCP broker + mlua host.
- `plugins/nefor-tui/` — declarative TUI plugin (Rust): primitives + reconciler + line-diff renderer + Lua VM. The chat surface itself is Lua composition (`starter/chat.lua`).
- `plugins/mock-plugin/` — Claude CLI wrapper (Rust) emitting `cc.*` events (opt-in).
- `plugins/openai-provider/` — OpenAI-compatible HTTP provider (Ollama, real OpenAI, etc).
- `plugins/reasoner-graph/` — orchestrator graph engine.
- `plugins/tool-gate/` — tool advertisement + permission gate.
- `plugins/basic-tools/` — bundled tools (read_file, etc).
- `plugins/mock-plugin/` — scriptable peer for integration tests.
- `starter/init.lua` — default engine composition. Spawns the orchestrator stack + nefor-tui with chat.lua.
- `starter/chat.lua` — chat surface as a ~280-LOC Lua composition over `tui.*` primitives.
- `cli-config/init.lua` — same engine, no UI: `nefor plugin agentic-cli "<prompt>"`.

## Testing

- `cargo test --workspace` — all unit + crate tests. Fast, hermetic: no network, no `claude`, no TTY.
- Manual TTY smoke (above) drives the real `claude` CLI end-to-end; needs `claude` on `$PATH` and makes live API calls.
