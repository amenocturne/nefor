-- echo.lua — echoes every event received back onto the bus.
--
-- Useful for testing round-trip delivery: send a peer plugin's event and
-- watch it come back stamped with `mock-plugin.echo` so you can tell the
-- original from the echo.

nefor.on_any(function(body, env)
  -- Don't echo our own echoes — otherwise we'd loop forever the first
  -- time the engine's bus delivers our echo (it won't, per spec §6:
  -- senders don't see their own messages, but a compliant engine under
  -- test might not be, and safety helps).
  if body.kind == "mock-plugin.echo" then return end

  nefor.emit("echo", {
    echoed_kind = body.kind,
    echoed_from = env.from,
    echoed_ts = env.ts,
  })
end)

nefor.on_shutdown(function()
  nefor.log("mock-plugin(echo): shutting down")
end)
