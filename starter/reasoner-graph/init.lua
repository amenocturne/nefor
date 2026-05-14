-- starter/reasoner-graph/init.lua — wrapper actor for the
-- reasoner-graph Rust binary.
--
-- ## Translation
--
-- None. The wrapper is identity — verbatim delivery to/from the
-- binary. The Rust binary speaks the canonical tool contract
-- (`tool.invoke{name="spawn_graph"}` / `tool.result`).
--
-- Replay-window suppression: reasoner-graph is stateful (tracks
-- in-flight runs, firing→node maps). During session resume the
-- replay envelopes go to pure-Lua actors only; we skip delivery to
-- this binary so it doesn't re-run completed graph nodes.

local json = nefor.json
local replay_window = require("lib.replay_window")

local NAME = "reasoner-graph"

local M = {}

function M.spawn_spec(command)
  assert(type(command) == "table", "reasoner-graph.spawn_spec: command required")

  local function from_plugin(env)
    if type(env.body) ~= "table" then return end
    nefor.engine.send(json.encode({
      type = "event",
      from = env.from or NAME,
      ts   = nefor.engine.now(),
      body = env.body,
    }))
  end

  local function to_plugin(env)
    if replay_window.active() then return end
    if env.from == NAME then return end
    nefor.engine.deliver(NAME, json.encode(env))
  end

  return {
    name        = NAME,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
