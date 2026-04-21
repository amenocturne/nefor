# nefor-tui

Terminal frontend plugin for [nefor](../../README.md). Speaks
[NCP v0.1](../../protocol/v0.1/spec.md) over stdio; renders an engine-driven
cell grid with `ratatui` and forwards keyboard, paste, mouse, and resize
input onto the bus.

The plugin has no chat-specific knowledge: it does not know about messages,
tools, roles, or agents. It only renders cells and forwards input. The
chat-layer composition lives in a separate plugin (`nefor-chat`) that
publishes `nefor-tui.*` grid events.

## Run

Spawned by the engine as a subprocess over stdio. Direct invocation is
useful only for wire smoke-testing with a fake engine.

## Protocol (sub-protocol, NCP v0.1)

All kinds below live on `body.kind` inside `type: "event"` envelopes. The
plugin attaches under the name `nefor-tui` and uses NCP `protocol_version`
`"0.1"`.

### Events the plugin CONSUMES (engine → tui)

| kind                           | body                                                                                                                      |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------- |
| `nefor-tui.grid.resize`        | `{grid: u32, width: u32, height: u32}`                                                                                    |
| `nefor-tui.grid.clear`         | `{grid: u32}`                                                                                                             |
| `nefor-tui.grid.line`          | `{grid: u32, row: u32, col_start: u32, cells: [[text: string, hl_id?: u32, repeat?: u32]]}`                               |
| `nefor-tui.grid.cursor_goto`   | `{grid: u32, row: u32, col: u32}`                                                                                         |
| `nefor-tui.grid.scroll`        | `{grid: u32, top: u32, bot: u32, rows: i32}` — positive rows move content up (matches nvim semantics)                     |
| `nefor-tui.grid.flush`         | `{}` — render accumulated events                                                                                          |
| `nefor-tui.hl_attr_define`     | `{id: u32, rgb: {fg?: u32, bg?: u32, sp?: u32, bold?: bool, italic?: bool, underline?: bool, reverse?: bool}}`            |
| `nefor-tui.default_colors`     | `{fg: u32, bg: u32, sp: u32}`                                                                                             |

`grid` is present for future-proofing; MVP renders only `grid=1`. Events
targeting other grids are silently ignored. Colors are 24-bit RGB packed
in `u32` (`0xRRGGBB`); absent highlight fields inherit from
`default_colors`.

### Events the plugin PRODUCES (tui → bus)

| kind                      | body                                                                                                                |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `nefor-tui.input.key`     | `{key: string, modifiers: string[]}`                                                                                |
| `nefor-tui.input.paste`   | `{text: string}` — bulk paste (bracketed paste)                                                                     |
| `nefor-tui.input.mouse`   | `{action: string, button?: string, row: u32, col: u32, modifiers: string[]}`                                        |
| `nefor-tui.input.resize`  | `{cols: u32, rows: u32}` — emitted on SIGWINCH and once at startup                                                  |
| `nefor-tui.ready`         | `{cols: u32, rows: u32}` — emitted once, immediately after `attach_ok`, to declare the plugin is rendering          |

`key` is the main key symbol: single-character strings like `"a"` /
`"A"` / `"!"`, or descriptive names like `"enter"`, `"backspace"`,
`"escape"`, `"tab"`, `"backtab"`, `"left"`, `"right"`, `"up"`, `"down"`,
`"pageup"`, `"pagedown"`, `"home"`, `"end"`, `"delete"`, `"insert"`,
`"f1"`..`"f12"`. Shift + letter produces an uppercase `key` string **and**
`"shift"` in modifiers. Pure modifier-only presses and key releases are not
forwarded.

`modifiers` is a subset of `["shift", "ctrl", "alt", "super"]`, ordered
deterministically.

`action` for mouse is `"down" | "up" | "drag" | "scroll_up" |
"scroll_down" | "scroll_left" | "scroll_right"`. `button` is
`"left" | "right" | "middle"` for button events; absent for scroll.

### Lifecycle

1. Connect stdio; send NCP `attach { name: "nefor-tui", version: "0.1.0", protocol_version: "0.1" }`.
2. Wait for `attach_ok` on stdin. On `error`, log (stderr) and exit 1.
3. Enter raw mode + alt screen + mouse capture + bracketed paste. A
   `Drop`-based guard restores the terminal on normal exit, error, or
   panic.
4. Emit `nefor-tui.input.resize` and then `nefor-tui.ready` with the
   measured terminal dimensions.
5. Main loop: multiplex (a) stdin NCP messages, (b) crossterm events,
   (c) SIGWINCH via crossterm's `Event::Resize`.
6. On engine `shutdown`, send `detach` and exit.

### Not in scope

- Chat, messages, tools, or agents. The engine's own `nefor-chat` plugin
  (or any other composition plugin) owns those concerns and publishes
  `nefor-tui.*` events at render time.
- Scrollback. Scroll is a publisher-side operation via `grid.scroll`.
- Modes or keymaps. Every forwardable key is emitted verbatim.
- Theming. Colors come entirely from `hl_attr_define` / `default_colors`.
