# nefor

Rust rewrite of the nefor agent harness. TUI agent runtime + context-combinators library.

**Status: MVP complete.** A minimal Claude Code wrapper. Launch with `just run` and chat with Claude in a TUI. DAG orchestration, permission-gate, and review-flow are post-MVP.


## Quick start

```sh
just setup   # cargo fetch
just run
just test
```

## Layout

- `crates/nefor-combinators/` — pure Rust substrate (Context, Transform, combinators).
- `crates/nefor/` — binary: TUI + Lua plugin host.
- `plugins/` — Lua plugins (mock-plugin, etc.).
- `starter/` — reference `init.lua`; copy what you want.

## Testing

- `cargo test --workspace` — all unit tests plus the starter/plugin Lua parse
  smoke (`crates/nefor/tests/starter_smoke.rs`). Fast, hermetic: no network,
  no `claude`, no TTY.
- `cargo test -p nefor --test mock_plugin -- --ignored` — end-to-end
  integration test that drives the real `claude` CLI through mock-plugin. The
  TUI's `enable_raw_mode` needs a real TTY, so run this from an interactive
  terminal. Requires `claude` on `PATH` and makes live API calls (real
  tokens / real money / real rate limits).
