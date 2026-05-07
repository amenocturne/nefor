-- Scrollable integration scenario.
--
-- A long virtual log + a status text + a controlled-scroll button:
-- - Mouse wheel scrolls the log when the cursor is over it; bubbles
--   otherwise.
-- - PgUp / PgDn / Home / End bubble to Lua (per spec — keyboard scrolling
--   is Lua's domain). Lua translates them into `tui.scroll_by` / etc.
-- - `stick_to = "end"` keeps the log pinned at the bottom through growth,
--   chat-transcript style.
-- - Lua tracks `scroll_offset` via the `on_scroll` callback so the status
--   line reflects the current position.

tui.start {
  initial_state = {
    rows          = 30,
    scroll_offset = 0,
  },
  view = function(s)
    local kids = {}
    for i = 1, s.rows do
      kids[#kids + 1] = tui.text { content = "row " .. i }
    end
    return tui.column { gap = 0, children = {
      tui.text { content = "offset: " .. tostring(s.scroll_offset) },
      tui.expanded {
        child = tui.scrollable {
          key        = "log",
          child      = tui.column { gap = 0, children = kids },
          stick_to   = "end",
          on_scroll  = "log.scrolled",
          scrollbar  = "auto",
          selectable = true,
        },
      },
    }}
  end,
  update = function(msg, s)
    if msg.kind == "log.scrolled" then
      return {
        rows          = s.rows,
        scroll_offset = msg.offset,
      }, {}
    elseif msg.kind == "key.pageup" then
      tui.scroll_by("log", -10)
      return s, {}
    elseif msg.kind == "key.pagedown" then
      tui.scroll_by("log", 10)
      return s, {}
    elseif msg.kind == "key.home" then
      tui.scroll_to("log", 0)
      return s, {}
    elseif msg.kind == "key.end" then
      tui.scroll_into_view("log")
      return s, {}
    end
    return s, {}
  end,
}
