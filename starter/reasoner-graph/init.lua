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

local NAME = "reasoner-graph"

local M = {}

function M.spawn_spec(command)
  assert(type(command) == "table", "reasoner-graph.spawn_spec: command required")

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      if type(env.body) == "table" then
        nefor.engine.send(json.encode({
          type = "event",
          from = env.from or NAME,
          ts   = nefor.engine.now(),
          body = env.body,
        }))
      end
    end
  end

  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if not env.replay and env.from ~= NAME then
        -- Strip framework-only fields (`replay`, …) when encoding for
        -- the wire — the protocol parser rejects unknown fields.
        nefor.engine.deliver(NAME, json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        }))
      end
    end
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
