-- minimal.lua — smallest useful mock-plugin scenario.
--
-- On ready_ok: emit a greeting event.
-- On shutdown: log and exit cleanly.

nefor.on_ready_ok(function()
  nefor.log("mock-plugin(minimal): ready; saying hello")
  nefor.emit("hello", { greeting = "hi from mock-plugin" })
end)

nefor.on_shutdown(function()
  nefor.log("mock-plugin(minimal): shutting down")
end)
