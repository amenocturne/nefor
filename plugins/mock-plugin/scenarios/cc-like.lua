-- cc-like.lua — simulates a mock-plugin-style streaming response.
--
-- After ready_ok, emits a sequence of `mock-plugin.delta` events carrying
-- text chunks, then a terminal `mock-plugin.result` event. Spaced with
-- nefor.sleep so observers see real inter-event timing.

local chunks = {
  "Hello",
  ", ",
  "world",
  "! ",
  "This ",
  "is ",
  "a ",
  "mock ",
  "stream.",
}

nefor.on_ready_ok(function()
  nefor.log("mock-plugin(cc-like): streaming " .. #chunks .. " deltas")
  for i, text in ipairs(chunks) do
    nefor.emit("delta", { seq = i, text = text })
    nefor.sleep(25)
  end
  nefor.emit("result", {
    status = "ok",
    text = table.concat(chunks),
  })
end)

nefor.on_shutdown(function()
  nefor.log("mock-plugin(cc-like): shutting down")
end)
