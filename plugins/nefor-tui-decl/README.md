# nefor-tui-decl

Declarative TUI plugin for nefor — Phase 1 of the rewrite that will eventually
replace `plugins/nefor-tui` and collapse `plugins/nefor-chat` into a Lua
composition layer.

## Status: phase 1

Foundations only. Not yet wired into `starter/init.lua`; the legacy
`nefor-tui` plugin keeps running as the chat frontend until phase 6.

What ships:

- Tree reconciler with `(type_tag, key)` matching, depth-first
  unmount/mount/update, instance-state preservation across rebuilds.
- Line-diff renderer wrapped in DEC mode 2026 (synchronized output) with
  full-frame on first render and after resize.
- Three primitives: `tui.text`, `tui.column`, `tui.padding`.
- Lua FFI: `tui.start { initial_state, view, update }` + the three
  primitive constructors.
- Raw key bubbling to Lua as `{ kind = "key.<name>", mods = {...} }`.
- In-process `Engine` integration test driving the counter scenario.

What's deferred:

- `row`, `expanded`, `spacer`, `constrained`, `align`, `stack` primitives
  (phase 2).
- `anchored` positioning (phase 3).
- `text_input` + input router (phase 4).
- `scrollable`, `markdown`, `spans`, `animation` (phase 5).
- Wide-char column accounting via `unicode-width` is plumbed but
  phase-1 tests only cover ASCII; documented gap.
- Side-effects beyond `{ kind = "exit" }` are tracing-warned and dropped.

## Quick run

The phase-1 binary loads a hard-coded counter scenario so the plugin can
be smoke-tested standalone before NCP-side wiring lands. The legacy
`nefor-tui` is what `starter/init.lua` still spawns; this binary is
exercised via tests and direct invocation only.

```sh
cargo test -p nefor-tui-decl
```

## Layout

| File | Role |
|------|------|
| `src/desc.rs` | Widget descriptions; Lua-table → `WidgetDescription` parser. |
| `src/instance.rs` | Reconciler-owned instance tree types + key composition. |
| `src/reconciler.rs` | `(type_tag, key)` match, mount / reuse / unmount. |
| `src/layout.rs` | Top-down sizing for `text`, `column`, `padding`. |
| `src/render.rs` | Line-diff renderer + frame buffer. |
| `src/ansi.rs` | CSI helpers (sync output, SGR, cursor moves). |
| `src/lua_host.rs` | mlua VM, `tui.*` install, view/update dispatch. |
| `src/input.rs` | Crossterm `KeyEvent` → engine `KeyMessage`. |
| `src/engine.rs` | State machine — owns reconciler + renderer + lua. |
| `src/ncp.rs` | NCP stdio transport. |
| `src/tty.rs` | `/dev/tty` open + `RawModeGuard`. |
| `src/main.rs` | Binary entrypoint: NCP handshake + crossterm event loop. |
| `tests/engine_test.rs` | In-process integration test (counter scenario). |
| `scenarios/counter.lua` | Reference Lua app — increments on `space`, exits on `q`. |
