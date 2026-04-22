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

Manual smoke against real Claude:

```sh
cargo build -p nefor -p mock-plugin -p nefor-chat -p nefor-tui
NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./tmp/smoke-config-m2
```

Ctrl+C (or Ctrl+D) exits cleanly; `/resume` loads the most recent prior session for the current cwd, `/resume <uuid>` loads a specific one.

## Layout

- `crates/nefor-combinators/` — pure Rust substrate (Context, Reasoner, combinators).
- `crates/nefor-protocol/` — NCP v0.1 types + parsers.
- `crates/nefor/` — engine binary: NCP broker + mlua host.
- `plugins/nefor-tui/` — terminal frontend (Rust, crossterm + ratatui).
- `plugins/nefor-chat/` — chat UI (Rust) bridging mock-plugin ↔ nefor-tui.
- `plugins/mock-plugin/` — Claude CLI wrapper (Rust) emitting `cc.*` events.
- `plugins/mock-plugin/` — scriptable peer for integration tests.
- `starter/init.lua` — legacy reference config (awaiting rewrite for the three-plugin graph).

## Testing

- `cargo test --workspace` — all unit + crate tests. Fast, hermetic: no network, no `claude`, no TTY.
- Manual TTY smoke (above) drives the real `claude` CLI end-to-end; needs `claude` on `$PATH` and makes live API calls.
