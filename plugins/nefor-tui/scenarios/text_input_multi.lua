-- Multi-line text-input scenario.
--
-- Same shape as text_input.lua but `max_lines = 6` so the input grows
-- vertically up to the cap and then internal-scrolls. Used by the
-- bottom-anchor regression test: pasting content past the cap must
-- leave the cursor row visible (Claude-style), not the top of the buffer.

tui.start {
  initial_state = {
    input_value = "",
    focused_id  = "input",
  },
  view = function(s)
    return tui.column { gap = 0, children = {
      tui.text_input {
        key       = "input",
        value     = s.input_value,
        focused   = s.focused_id == "input",
        on_change = "input.changed",
        on_submit = "input.submit",
        min_lines = 1,
        max_lines = 6,
      },
    } }
  end,
  update = function(msg, s)
    if msg.kind == "input.changed" then
      return { input_value = msg.value, focused_id = s.focused_id }, {}
    end
    return s, {}
  end,
}
