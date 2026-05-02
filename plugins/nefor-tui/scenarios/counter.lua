tui.start {
  initial_state = { count = 0 },
  view = function(s)
    return tui.column { gap = 0, children = {
      tui.padding { value = 1, child = tui.text { content = "count: " .. tostring(s.count) } },
    }}
  end,
  update = function(msg, s)
    if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
    if msg.kind == "key.q"     then return s, { { kind = "exit" } } end
    return s, {}
  end,
}
