# nefor-chat

Chat UI plugin for [nefor](../../README.md). Bridges `mock-plugin` (the
Claude Code wrapper) and `nefor-tui` (the cell-grid renderer) over
[NCP v0.1](../../protocol/v0.1/spec.md). Owns the chat-layer state —
transcript, input buffer, scroll offset — and translates both sides into
grid mutations published on the bus.

The plugin never touches the terminal directly; every rendering concern is
expressed as a `nefor-tui.*` event.

## Layout

```
+------------------------------------+
| transcript (scrollable, wraps)     |
| you> hello                         |
| claude> hi! how can I help?        |
| [tool: read]                       |
| ...                                |
+------------------------------------+
| > _                                |  <-- input line (row = rows-1)
+------------------------------------+
```

Transcript occupies rows `0..(rows-1)`. Input line is the last row.

## Run

Spawned by the engine as a subprocess over stdio. Add an entry to your
`init.lua`:

```lua
nefor.plugins.spawn {
  name    = "nefor-chat",
  command = { "./target/debug/nefor-chat" },
}
```

Logs go to stderr (`tracing`, env filter default `info`); stdout is the
NCP channel.

## Events

### Consumes

From `nefor-tui`:

| kind                         | effect                                                         |
| ---------------------------- | -------------------------------------------------------------- |
| `nefor-tui.ready`            | Unblocks rendering; records initial cols/rows                  |
| `nefor-tui.input.resize`     | Updates dimensions and re-renders                              |
| `nefor-tui.input.key`        | Dispatches to buffer / cursor / scroll / submit behaviour      |
| `nefor-tui.input.paste`      | Inserts paste text at cursor                                   |

Key names (from nefor-tui's `key_body`): `enter`, `backspace`, `left`,
`right`, `home`, `end`, `pageup`, `pagedown`, `escape`; single-char keys
(`"a"`, `"A"`, `"!"`, `" "`, …) become literal input characters. Ctrl-
modified keys are suppressed so `Ctrl+C` never leaks into the buffer.

From `mock-plugin`:

| kind                    | effect                                                              |
| ----------------------- | ------------------------------------------------------------------- |
| `cc.stream.delta`       | Appends `text` to the current streaming assistant entry             |
| `cc.stream.end`         | Finalizes the assistant entry (replaces text if `text` is present)  |
| `cc.tool.start`         | Appends `[tool: <name>]` as a system entry                          |
| `cc.turn.error`         | Appends `[error: <message>]` as a system entry                      |
| `cc.hello` / `cc.ready` | Logged at debug; no UI effect                                       |
| `cc.goodbye`            | Logged at debug; no UI effect                                       |

From the engine:

| kind       | effect             |
| ---------- | ------------------ |
| `shutdown` | Exit the main loop |

### Produces

To `nefor-tui`:

| kind                           | when                                                             |
| ------------------------------ | ---------------------------------------------------------------- |
| `nefor-tui.default_colors`     | Once, after the first `nefor-tui.ready`                          |
| `nefor-tui.hl_attr_define`     | Once per palette entry, after `default_colors`                   |
| `nefor-tui.grid.clear`         | Start of every frame                                             |
| `nefor-tui.grid.line`          | One per transcript row + one for the input row                   |
| `nefor-tui.grid.cursor_goto`   | Positions the cursor inside the input line                       |
| `nefor-tui.grid.flush`         | End of every frame — commits the redraw                          |

To `mock-plugin`:

| kind         | when                             |
| ------------ | -------------------------------- |
| `cc.prompt`  | User pressed Enter on a non-empty buffer. `body: {text}` |

Lifecycle:

| kind                  | when                              |
| --------------------- | --------------------------------- |
| `nefor-chat.hello`    | After `ready_ok`                  |
| `nefor-chat.goodbye`  | Best-effort, before exit          |

## Rendering strategy

Full redraw on every state change. Each state mutation triggers the
sequence:

1. `grid.clear`
2. `grid.line` for every row in `0..(rows-1)` — either wrapped transcript
   text or a blank padded line
3. `grid.line` for the input row
4. `grid.cursor_goto` positioning the cursor inside the input line
5. `grid.flush`

No diffing. Fast enough for chat velocities; optimize later if needed.

## Palette

Five plugin-local highlight IDs are defined at startup:

| ID | name       | role                              |
| -: | ---------- | --------------------------------- |
|  1 | user       | `you>` prefix + user message body |
|  2 | assistant  | `claude>` prefix + assistant body |
|  3 | system     | `[tool: …]` / `[error: …]` lines  |
|  4 | input      | `> ` prefix + buffer contents     |
|  5 | status     | reserved (unused in v1)           |

## Scope

**In**:
- Plain-text transcript (no markdown)
- Input line editing (insert, backspace, cursor nav, home/end, paste)
- Word-wrap with unicode-width correctness
- Page-based scrollback (pageup / pagedown)
- Streaming assistant deltas with `cc.stream.end` reconciliation

**Out (v1)**:
- Markdown / syntax highlighting
- History recall (up/down arrows)
- Interrupt / Esc → `cc.interrupt` (deferred to v2)
- Themes beyond the plugin's default palette
