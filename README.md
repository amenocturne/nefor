# nefor

Rust rewrite of the nefor agent harness. TUI agent runtime + context-combinators library.

**Status: MVP in progress.** Nothing ships yet. The MVP stops at a minimal Claude Code wrapper — a TUI that spawns `claude` and streams its output via a Lua plugin.


## Quick start

```sh
just setup   # cargo fetch
just run     # (once MVP is built)
just test
```

## Layout

- `crates/nefor-combinators/` — pure Rust substrate (Context, Transform, combinators).
- `crates/nefor/` — binary: TUI + Lua plugin host.
- `plugins/` — Lua plugins (mock-plugin, etc.).
- `starter/` — reference `init.lua`; copy what you want.
