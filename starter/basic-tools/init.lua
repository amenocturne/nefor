-- starter/basic-tools/init.lua — wrapper actor for the basic-tools
-- Rust binary.
--
-- ## Translation
--
-- None. basic-tools speaks the canonical tool contract directly
-- (`tool.invoke` / `tool.result` / `tool.gate.advertise` /
-- `tool.gate.request`). The wrapper's `to_plugin` is just verbatim
-- delivery, with replay-window suppression so a session resume doesn't
-- redrive the binary with replayed envelopes.
--
-- ## Phase 2: tool-contract declaration
--
-- basic-tools is a tool consumer (advertises tools to the gate, emits
-- tool results). It depends on the canonical `generic-tool.ToolCalls`/
-- `generic-tool.ToolResults` type tags being declared against
-- nefor-combinators. We declare them from Lua via the tool contract
-- lib (the generic-tool Rust binary's startup envelope). `declare()`
-- is idempotent and timing-safe (eagerly emits at load; combinators
-- picks up the registration via ncp.lua's replay-on-attach when it
-- readies).

local json = nefor.json

local config        = require("config")
local tool_contract = require("lib.contracts.tool")
local replay_window = require("lib.replay_window")

tool_contract.declare()

local NAME = "basic-tools"

local function from_plugin(env)
  -- Republish verbatim onto the bus.
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
  command     = { config.bin("basic-tools"), "--gate", "tool-gate" },
  from_plugin = from_plugin,
  to_plugin   = to_plugin,
  receive_msg = function(_entry) end,
}
