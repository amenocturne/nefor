-- nefor-tui widget library. Widgets render bus-agnostic UI primitives the
-- consumer composes into their own surface — they take data via opts and
-- zero-arg source functions; they never reach into I/O.

local M = {}

M.util = require("nefor-tui.util")

M.widget = {
  prompt    = require("nefor-tui.widget.prompt"),
  chat      = require("nefor-tui.widget.chat"),
  text_pane = require("nefor-tui.widget.text_pane"),
  popup     = require("nefor-tui.widget.popup"),
  picker    = require("nefor-tui.widget.picker"),
}

return M
