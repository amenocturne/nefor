-- lua/libs/compositors/chat_bridge.lua — engine-side actor that bridges
-- the bus to the nefor-tui plugin binary's stdio. Identity passthrough
-- on both directions: the binary's outbound envelopes go on the bus
-- verbatim; inbound envelopes are re-encoded without the framework-only
-- `replay` flag (the protocol parser at the binary rejects unknown
-- fields) but with `env.replay` envelopes preserved so chat/init.lua
-- can rebuild its transcript on resume.
--
-- The companion UI script lives at `starter/chat/init.lua` and is
-- loaded by the nefor-tui binary itself; the two run in different
-- processes and share no Lua state.

local json = nefor.json

local M = {}

function M.spawn_spec(command)
  if type(command) ~= "table" then
    error("chat_bridge.spawn_spec: command must be a table, got " .. type(command))
  end
  local name = "nefor-tui"

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      if type(env.body) == "table" then
        nefor.engine.send(json.encode({
          type = "event",
          from = env.from or name,
          ts   = nefor.engine.now(),
          body = env.body,
        }))
      end
    end
  end

  local function to_plugin(envs)
    local payloads = {}
    local contains_replay = false
    for _, env in ipairs(envs) do
      if env.from ~= name then
        payloads[#payloads + 1] = json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        })
        if env.replay == true then contains_replay = true end
      end
    end

    if contains_replay then
      -- Protect the complete dispatch batch, not just entries stamped
      -- replay=true. `sessions.replay.end` flips the stamp before it is
      -- appended, and resume completion follows it; both must stay ordered
      -- behind the replay they frame even when the TUI consumes slowly.
      nefor.engine.deliver_batch(name, payloads)
      return
    end

    for _, payload in ipairs(payloads) do
      nefor.engine.deliver(name, payload)
    end
  end

  return {
    name        = name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
