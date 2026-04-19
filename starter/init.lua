-- Reference init.lua.
--
-- Not auto-installed. Copy to ~/.config/nefor/init.lua and adapt.
-- Proves the three binding families introduced in this commit work end
-- to end: a widget, a key handler, and a subprocess.
--
-- See CLAUDE.md for the MVP scope — mock-plugin and the rest of the
-- starter bundle arrive in follow-up commits.

-- A trivial status widget pinned to the bottom row. The renderer returns
-- the lines ratatui will draw; it's called once per frame, so it must be
-- cheap.
local status_lines = { "nefor starter — press q or Ctrl-C to quit" }

nefor.ui.register_widget(
  { kind = "bottom", size = 1 },
  function()
    return status_lines
  end
)

-- A second widget occupies the center pane.
local hint_lines = {
  "nefor",
  "",
  "you are looking at starter/init.lua.",
  "",
  "try pressing '?' to toggle the hint.",
  "the subprocess below will echo its pid once at startup.",
}

local show_hint = true
nefor.ui.register_widget(
  { kind = "center" },
  function()
    if show_hint then
      return hint_lines
    else
      return { "(hint hidden — press ? again)" }
    end
  end
)

-- '?' toggles the hint.
nefor.ui.subscribe_key("?", function(ev)
  show_hint = not show_hint
  nefor.log.info("toggled hint", { shown = show_hint })
end)

-- Spawn a subprocess at startup just to exercise the binding. Echoes its
-- own pid via the shell, captures the line, and shows it in the status
-- widget.
nefor.process.spawn({
  cmd = "sh",
  args = { "-c", "echo pid:$$" },
  on_stdout = function(line)
    status_lines = { line .. " — starter init.lua ok" }
    nefor.log.info("subprocess line", { line = line })
  end,
  on_exit = function(code)
    nefor.log.info("subprocess exit", { code = code })
  end,
})
