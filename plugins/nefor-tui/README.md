# nefor-tui

Declarative TUI plugin for nefor — Rust engine + reconciler + Lua-driven
primitives. Chat is composed as a Lua module (`starter/chat.lua`) that
runs inside this plugin's Lua VM. The legacy split (`nefor-chat` chat-
state owner + ratatui-based `nefor-tui` renderer over a grid protocol)
collapsed into this single plugin at phase 6 of the rewrite.

## What ships

- Tree reconciler with `(type_tag, key)` matching, depth-first
  unmount/mount/update, instance-state preservation across rebuilds.
- Line-diff renderer wrapped in DEC mode 2026 (synchronized output) with
  full-frame on first render and after resize.
- Primitives: `tui.text`, `tui.spans`, `tui.markdown`, `tui.animation`,
  `tui.column`, `tui.row`, `tui.stack`, `tui.padding`, `tui.expanded`,
  `tui.spacer`, `tui.constrained`, `tui.align`, `tui.anchored`,
  `tui.text_input`, `tui.scrollable`.
- Lua FFI: `tui.start { initial_state, view, update }` + primitive
  constructors + `tui.scroll_to / scroll_by / scroll_into_view /
  scroll_position` + NCP egress via the `send_to` side-effect and
  `nefor.bus.on_event(pattern, msg_kind)` for ingress.
- Raw key bubbling to Lua as `{ kind = "key.<name>", mods = {...} }`;
  mouse wheel auto-scrolls the scrollable under the cursor.

## CLI flags

```
nefor-tui --script <path-to-lua>
```

`--script` (or `-s`) loads a user-authored Lua module that calls
`tui.start { ... }`. The shipped chat surface lives at
`starter/chat.lua`. Without `--script`, a built-in counter scenario
loads (useful for `cargo run -p nefor-tui` smoke runs).

## Quick run

```sh
cargo test -p nefor-tui
```

The full chat surface comes up via the engine launcher:

```sh
NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter
```

## Layout

| File | Role |
|------|------|
| `src/desc.rs` | Widget descriptions; Lua-table → `WidgetDescription` parser. |
| `src/instance.rs` | Reconciler-owned instance tree types + key composition. |
| `src/reconciler.rs` | `(type_tag, key)` match, mount / reuse / unmount. |
| `src/layout.rs` | Constraints-down / sizes-up two-pass layout for every primitive. |
| `src/render.rs` | Line-diff renderer + frame buffer. |
| `src/ansi.rs` | CSI helpers (sync output, SGR, cursor moves). |
| `src/lua_host.rs` | mlua VM, `tui.*` install, view/update dispatch, NCP bus bridge. |
| `src/input.rs` | Crossterm `KeyEvent` → engine `KeyMessage`. |
| `src/input_router.rs` | Editing-keys-to-focused-text_input vs bubble-to-Lua. |
| `src/mouse.rs` | Hit-test + auto-wheel-scroll routing. |
| `src/scrollable.rs` | Scroll state + wheel-step constants. |
| `src/text_input.rs` | Single-line + multiline edit state, IME, paste. |
| `src/markdown.rs` | pulldown-cmark adapter for `tui.markdown`. |
| `src/animation.rs` | Time-based frame sampler. |
| `src/engine.rs` | State machine — owns reconciler + renderer + lua + NCP queue. |
| `src/ncp.rs` | NCP stdio transport. |
| `src/tty.rs` | `/dev/tty` open + `RawModeGuard`. |
| `src/main.rs` | Binary entrypoint: NCP handshake + crossterm event loop + `--script` flag. |
| `tests/*_test.rs` | In-process integration tests for engine, layout, scrollable, text_input, animation. |
| `scenarios/*.lua` | Standalone Lua apps for direct inspection. |
