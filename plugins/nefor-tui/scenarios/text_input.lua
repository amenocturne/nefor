-- Text-input integration scenario.
--
-- Lua holds the value (controlled component); the engine routes editing
-- keys to the focused input and dispatches `on_change` / `on_submit`
-- callbacks back through `update`. Tab bubbles as a raw `key.tab`
-- message, demonstrating that the input doesn't swallow user shortcuts.

tui.start {
  initial_state = {
    input_value     = "",
    submitted_value = nil,
    tab_count       = 0,
    focused_id      = "input",
  },
  view = function(s)
    return tui.column { gap = 0, children = {
      tui.text { content = "value: " .. s.input_value },
      tui.text {
        content = "submitted: " .. tostring(s.submitted_value or "<nil>"),
      },
      tui.text { content = "tabs: " .. tostring(s.tab_count) },
      tui.text_input {
        key       = "input",
        value     = s.input_value,
        focused   = s.focused_id == "input",
        on_change = "input.changed",
        on_submit = "input.submit",
        min_lines = 1,
        max_lines = 1,
      },
    } }
  end,
  update = function(msg, s)
    if msg.kind == "input.changed" then
      return {
        input_value     = msg.value,
        submitted_value = s.submitted_value,
        tab_count       = s.tab_count,
        focused_id      = s.focused_id,
      }, {}
    elseif msg.kind == "input.submit" then
      return {
        input_value     = s.input_value,
        submitted_value = msg.value,
        tab_count       = s.tab_count,
        focused_id      = s.focused_id,
      }, {}
    elseif msg.kind == "key.tab" then
      return {
        input_value     = s.input_value,
        submitted_value = s.submitted_value,
        tab_count       = s.tab_count + 1,
        focused_id      = s.focused_id,
      }, {}
    end
    return s, {}
  end,
}
