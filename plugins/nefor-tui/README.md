# nefor-tui

Declarative TUI plugin for nefor — Rust engine + reconciler + Lua-driven
primitives. Chat is composed as a Lua module (`examples/nefor-agent/chat/init.lua`) that
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

## OS operations from Lua

These generic effects do not interpret rendered links or select UI policy. Lua
composition decides when to call them and how to display failures.

```lua
local ok, err = tui.open_external { uri = "mailto:person@example.org" }
local ok, err = tui.open_external { path = "notes/design.md", base = "/absolute/project" }
local ok, err = tui.open_external { path = "/absolute/project/image.png" }
local ok, err = tui.copy_to_clipboard("text to copy")
```

- `tui.open_external(spec)` accepts exactly one target field: `uri` or `path`.
  Targets must be non-empty UTF-8 strings without NUL bytes. `uri` is passed
  unchanged to the platform opener: there is no URL normalization, scheme
  allowlist, browser restriction, or application selection. `base` applies only
  to `path`; relative paths require an explicit absolute `base`. Paths are
  joined to that base without filesystem canonicalization, existence checks,
  tilde expansion, or dependence on the plugin's working directory. Absolute
  paths do not require a base; any supplied base must still be absolute.
- The opener uses the platform's default handling for URIs and files (`open`
  on macOS, `explorer.exe` on Windows, `xdg-open` on other Unix platforms).
  Arguments are passed directly to a structured process command, never a shell.
  The call waits for the opener's exit status, not for the launched application
  to finish. Success means that the opener exited successfully, not that the
  destination was loaded or viewed. Unsupported platforms, launch failures,
  and non-success exit statuses are reported as failures.
- `tui.copy_to_clipboard(text)` accepts a UTF-8 string (including an empty
  string) and writes it through the system clipboard backend. Success means
  that the backend accepted the write; subsequent clipboard ownership and
  persistence follow that backend's platform behavior.
- Both calls return `true` on success, or `nil, error_string` for invalid
  arguments and expected OS/backend failures, rather than raising a Lua
  exception. Existing clipboard callers may continue ignoring the returns.
  Error strings are diagnostic text, not stable machine-readable categories.

The Rust OS-effects boundary is substituted with a fake in unit tests, so API
validation, target preservation, command construction, and failure reporting
are tested without launching applications or accessing the real clipboard.

## CLI flags

```
nefor-tui --script <path-to-lua>
```

`--script` (or `-s`) loads a user-authored Lua module that calls
`tui.start { ... }`. The shipped chat surface lives at
`examples/nefor-agent/chat/init.lua`. Without `--script`, a built-in counter scenario
loads (useful for `cargo run -p nefor-tui` smoke runs).

## Quick run

```sh
cargo test -p nefor-tui
```

## Snapshot tests

Integration tests can assert exact visual output by driving the engine
in-process and reading [`Engine::snapshot`]. Each row of the framebuffer
is `width` cells wide (trailing whitespace preserved); rows are joined
with `\n`.

```rust
use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;

const SCENARIO: &str = r#"
    tui.start {
      initial_state = { count = 0 },
      view = function(s)
        return tui.text { content = "count: " .. tostring(s.count) }
      end,
      update = function(msg, s)
        if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
        return s, {}
      end,
    }
"#;

let mut engine = Engine::new(12, 1).unwrap();
engine.load_scenario(SCENARIO).unwrap();
let _ = engine.render_if_dirty().unwrap();
assert_eq!(engine.snapshot(), "count: 0    ");

engine.handle_key(KeyMessage { name: "space".into(), mods: vec![] }).unwrap();
let _ = engine.render_if_dirty().unwrap();
assert_eq!(engine.snapshot(), "count: 1    ");
```

Three snapshot variants are available on `Engine`:

- `snapshot()` — plain text, no style info.
- `snapshot_ansi()` — text with inline ANSI SGR escapes (each row ends
  with a reset).
- `snapshot_styled()` — text with human-readable `[bold]...[/bold]`,
  `[italic]...[/italic]`, `[underline]...[/underline]` markers. Designed
  for readable golden diffs.

See `tests/snapshot_test.rs` for the canonical pattern.

The full chat surface comes up via the engine launcher:

```sh
cargo build --workspace
NEFOR_DEV_DIR=$PWD NEFOR_CONFIG_DIR=$PWD/examples/nefor-agent cargo run --bin nefor
```

## Layout

| File                  | Role                                                                                  |
| --------------------- | ------------------------------------------------------------------------------------- |
| `src/desc.rs`         | Widget descriptions; Lua-table → `WidgetDescription` parser.                          |
| `src/instance.rs`     | Reconciler-owned instance tree types + key composition.                               |
| `src/reconciler.rs`   | `(type_tag, key)` match, mount / reuse / unmount.                                     |
| `src/layout.rs`       | Constraints-down / sizes-up two-pass layout for every primitive.                      |
| `src/render.rs`       | Line-diff renderer + frame buffer.                                                    |
| `src/ansi.rs`         | CSI helpers (sync output, SGR, cursor moves).                                         |
| `src/lua_host.rs`     | mlua VM, `tui.*` install, view/update dispatch, NCP bus bridge.                       |
| `src/input.rs`        | Crossterm `KeyEvent` → engine `KeyMessage`.                                           |
| `src/input_router.rs` | Editing-keys-to-focused-text_input vs bubble-to-Lua.                                  |
| `src/mouse.rs`        | Hit-test + auto-wheel-scroll routing.                                                 |
| `src/scrollable.rs`   | Scroll state + wheel-step constants.                                                  |
| `src/text_input.rs`   | Single-line + multiline edit state, IME, paste.                                       |
| `src/markdown.rs`     | pulldown-cmark adapter for `tui.markdown`.                                            |
| `src/animation.rs`    | Time-based frame sampler.                                                             |
| `src/engine.rs`       | State machine — owns reconciler + renderer + lua + NCP queue.                         |
| `src/ncp.rs`          | NCP stdio transport.                                                                  |
| `src/tty.rs`          | `/dev/tty` open + `RawModeGuard`.                                                     |
| `src/main.rs`         | Binary entrypoint: NCP handshake/stdio loop + crossterm event loop + `--script` flag. |
| `tests/*_test.rs`     | In-process integration tests for engine, layout, scrollable, text_input, animation.   |
| `scenarios/*.lua`     | Standalone Lua apps for direct inspection.                                            |

# Starter TUI

This guide describes the chat interface shipped in `examples/nefor-agent/`. It is not a contract for every Nefor configuration: the TUI engine supplies widgets, input, rendering, selection, clipboard-image handling, and link activation, while the starter Lua composition chooses the commands, keys, workflow controls, session behavior, and permission policy documented here.

## The screen

The main pane is a Markdown transcript above a multiline prompt. The status line reports the active provider/model, reasoning effort, permission mode, context use, configured provider quota/cost values, and runtime state. `/usage` opens the configured provider-owned account values. Those values are separate from the context bar: quota and cost describe provider accounts or the current session, while context use describes the active model request. `Ctrl+B` opens a sidebar for live MAG workflows; `Tab` moves focus between the prompt and that sidebar. The sidebar reconstructs the logical node paths carried by compiled MAG artifacts, so compound stages expand recursively without deriving hierarchy from dotted runtime actor ids.

Send with `Enter`; insert a newline with `Shift+Enter`. While a turn is running, another submission is kept as one queued message. Repeated submissions coalesce into that queued entry rather than becoming separate pending turns. See [commands and keys](../../examples/nefor-agent/chat/slash.lua) for steering and interruption.

The transcript deliberately favors semantic summaries:

- tool calls render as compact **receipts** rather than raw payload dumps;
- reasoning and tool details are collapsed until `Ctrl+O`;
- `Ctrl+R` reveals raw data for the latest expanded tool receipt; expanded receipts show stable short numbers, and `/raw N` offers newest-first completion to target one;
- selecting transcript text with the mouse copies it to the clipboard and shows a toast.

## Paths, images, and links

Type `@path` anywhere in a message to include a text file. Completion is cwd-relative, one directory level at a time; `Tab` applies a candidate. Paths containing spaces are quoted by completion. At submission, readable text files are inlined for the agent, with content beyond 16 KiB marked as truncated. Missing paths remain literal. See [workflows and tools](../../examples/nefor-agent/docs/workflows.md#path-references).

Paste an image from the system clipboard with `Ctrl+V` or `Super+V`. The TUI saves it as a PNG and inserts its absolute path into the prompt. This gives the agent a path it can inspect with `read_image`; it does not itself attach image bytes to the user message. Text paste continues through the normal prompt path. Image interpretation still depends on a vision-capable provider/model.

Rendered Markdown destinations are preserved exactly as provided by the parser and delivered to optional user-owned Lua activation. The starter has no built-in open action: without a configured handler, clicking shows a visible no-action diagnostic. See the callback and extension contracts below.

## Sessions and context

Every real user submission is persisted to a JSONL session. `/new` (alias `/clear`) requests a fresh session and interrupts active work; the visible transcript and other session-scoped projections reset only after the sessions actor acknowledges the replacement. Process-level provider registrations, authentication, and configuration survive because the process is not restarted. `/resume` opens the recent-session picker; `/resume <id>` switches directly and reconstructs canonical conversation state from the recorded session without re-running historical work. See [sessions and context](../../examples/nefor-agent/docs/sessions.md) before sharing a session directory between installations.

`/compact` asks the active provider/model to compact its model context. The full persisted transcript remains the fallback, and compaction does not mean the transcript was deleted or exported.

## Permissions

The starter begins in `safe` unless this invocation explicitly selects another startup mode. `/safe`, `/auto`, and `/yolo` change the live mode. `yolo` accepts broad risk; `auto` runs without interactive prompts and denies ordinary requests that still need a human, although the current starter auto-approves its separate write-review plan gate. Read [permissions](../../examples/nefor-agent/docs/permissions.md): the UI presents three distinct approval systems, and their boundaries matter.

## More

- [Commands and keys](../../examples/nefor-agent/chat/slash.lua) — complete starter command and key reference
- [Sessions and context](../../examples/nefor-agent/docs/sessions.md) — persistence, replay, resume, and compaction
- [Workflows and tools](../../examples/nefor-agent/docs/workflows.md) — sidebar, termination, paths, images, receipts, and links
- [Permissions](../../examples/nefor-agent/docs/permissions.md) — safe/auto/yolo and all approval paths

## User-owned link activation

`tui.start` optionally accepts `on_link(destination, state)`. The destination
is the Markdown parser's verbatim `dest_url`, not a normalized URL: relative
paths, fragments, arbitrary app schemes, `file:` destinations, and empty
strings are metadata too. Wrapping, layout and hit testing retain it. An
unmodified left-button down/up on the same target invokes the callback;
drags, wheel events, intervening keys, changed modifiers/buttons, or release
outside that target cancel activation. Ordinary down-click dispatch and text
selection remain available.

The callback returns `next_state_table, effects`, using the same effect forms
as `update`; `nil` declines. It is optional at startup. Missing, declined,
invalid-return, or errored callbacks display `Link: no action` over the current
view, with diagnostic detail. The overlay clears on the next key or mouse
press. No OS action is implicit. This boundary catches callback errors, not
transactions: imperative effects already performed by a callback cannot be
rolled back, and callbacks should return replacement state rather than mutate
the supplied table before a possible error.

## Canonical chat extension events and popups

The canonical entry loads `config.active.chat_extension` (a table or module
name) through `libs.chat.extensions`. In addition to existing command and
status hooks, it supports:

- `on_event(msg, readonly_state, api) -> next_state, effects` (or `nil` to
  decline). Unclaimed events reach this hook through `libs.chat.controller`.
  The entry's `on_link` callback translates activation to
  `{kind = "link.activate", destination = verbatim_destination}` and calls
  the same hook. This is an event adapter, not a link action.
- `api.finish(patch)` constructs replacement state; `api.clear` clears a
  patched field. Store custom popup data in `extension_popup`, not `popup`.
  The latter is reserved for correlated canonical popups and cannot be patched
  by this helper.
- `popup_view(readonly_popup, readonly_state) -> widget_or_nil` renders that
  slot through `libs.chat.view`; use the generic `nefor-tui` popup widget or
  any other widget composition. While the slot exists, prompt input loses
  focus and key/mouse messages reach `on_event` before ordinary key handlers.

Canonical handlers for bus events still run first. When a canonical popup is
active, extension events, link activation and popup rendering are suppressed:
permission request correlation and approve/deny handling remain authoritative.
The extension popup is retained behind it and resumes after it closes. There
is no default extension popup or link menu.
