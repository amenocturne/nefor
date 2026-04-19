# starter/

Reference `init.lua` for a minimal Claude Code chat TUI.

**This is not auto-installed.** To use:

1. Copy `init.lua` to `~/.config/nefor/init.lua` (or your `NEFOR_APPNAME` equivalent).
3. Ensure `claude` is on `PATH` (the Claude Code CLI). Verify with `claude --version`.
4. Run `just run` from the monorepo root (or `cargo run --bin nefor`).
5. Type a prompt, press Enter. Ctrl-C quits.

## What's here

- Three widgets: title bar (top, 1 row), scrolling transcript (center), input + hint (bottom, 2 rows).
- A single `mock-plugin` session with `permission_mode = "bypassPermissions"` — adjust if you want CC to ask before running tools (see `cc.session.new` options).
- Streaming: assistant text accumulates into one `[assistant] ...` line as deltas arrive.
- Tool calls: shown inline as `[tool <name>] <short input hint>`.
- Final per-turn cost + duration printed after the response.

## Known MVP limitations

- **`q` is a hard quit.** The Rust binary's base event loop treats a plain `q` keypress as "quit" before user Lua handlers see it. Messages that start with a bare `q` will close the TUI. Workaround: lead with any other character, or use Ctrl-C deliberately. A chat-aware quit will land in v0.2.
- Transcript is capped at 500 lines; older lines are dropped silently from the front.
- No scrollback controls yet — the center widget shows whatever ratatui chooses to fit.
- No prompt history (up-arrow recall). That's a plugin job.
- One session per nefor launch; no multi-session switching.
- Byte-level Backspace: ASCII is fine, multibyte input gets truncated bytes on erase. Post-MVP.

## Extending

- Swap the `on_tool_start` / `on_turn_done` / `on_turn_error` callbacks in `init.lua` to route elsewhere — a logger, a status widget, etc.
- Subscribe to `nefor.events.on("cc:tool_start", ...)` if you want cross-plugin observers; mock-plugin emits string-payload bus events alongside the structured callbacks (see `plugins/mock-plugin/lua/events.lua`).
- Replace the input renderer with your own widget, or add new ones on `left` / `right` regions.
- Hook `Up` / `Down` in the key handler for prompt history; `Esc` to clear the draft.
