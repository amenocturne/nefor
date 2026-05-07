-- starter/nefor-combinators/init.lua — wrapper actor for the
-- nefor-combinators Rust binary.
--
-- ## Translation
--
-- None. The wrapper is identity — verbatim delivery to/from the
-- binary, with replay-window suppression so a session resume doesn't
-- redrive the binary with replayed envelopes.
--
-- Combinators is a stateful registry: it tracks types and `Into`
-- declarations across plugins and answers `combinators.query` /
-- `combinators.invoke`. During replay it would otherwise process
-- already-applied registrations a second time.

local json = nefor.json

local config        = require("config")
local replay_window = require("lib.replay_window")

local NAME = "nefor-combinators"

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
  command     = { config.bin("nefor-combinators") },
  from_plugin = from_plugin,
  to_plugin   = to_plugin,
  receive_msg = function(_entry) end,
}
