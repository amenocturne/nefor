-- starter/nefor-tui/init.lua — wrapper actor for the nefor-tui Rust
-- binary.
--
-- ## from_plugin (binary → bus)
--
-- Republish every TUI emission verbatim onto the bus. The agentic-loop
-- subscribes via `nefor.bus.on_event` and reacts; other wrappers
-- (provider, tool-gate) see the same envelopes and decide whether to
-- forward them to their peer. The Phase-3 architecture means the
-- provider wrapper's `to_plugin` no longer translates
-- `chat.input.submit` → `<prefix>.prompt` (the orchestration goes
-- through `tool.invoke` instead), so the previous "drop these kinds at
-- ingress" guard isn't needed.
--
-- ## to_plugin (bus → binary)
--
-- Deliver verbatim, skipping replay-window envelopes and
-- self-emissions. No translation needed — chat.message.append /
-- chat.stream.delta / etc. flow through to the TUI as-is.

local json = nefor.json
local replay_window = require("lib.replay_window")

local M = {}

function M.spawn_spec(command)
  assert(type(command) == "table", "nefor-tui.spawn_spec: command required")

  local function from_plugin(env)
    if type(env.body) ~= "table" then return end
    nefor.engine.send(json.encode({
      type = "event",
      from = env.from or "nefor-tui",
      ts   = nefor.engine.now(),
      body = env.body,
    }))
  end

  local function to_plugin(env)
    if replay_window.active() then return end
    if env.from == "nefor-tui" then return end
    nefor.engine.deliver("nefor-tui", json.encode(env))
  end

  return {
    name        = "nefor-tui",
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
