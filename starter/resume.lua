-- starter/resume.lua — per-plugin resume transform registry.
--
-- The user's architectural direction (verbatim):
--
--   "we are resuming session into a static state (it is not running any
--    tools/graphs and etc) so we just need to feed llm all the context that
--    would've been fed if we just continued this session, nothing more.
--    Maybe we need to implement per-plugin resume function that understands
--    what it should do? And that's actually purely lua thing. Maybe a small
--    lua lib for resuming, which has: a function that processes each
--    message + has functions defined per-plugin that modify messages in
--    their own way. We must not bake resume functionality into the engine
--    itself."
--
-- Resume is a static-state replay: when the engine boots with `nefor.parent_session`
-- pointing at a previous session, the saved log carries every event the
-- prior run emitted. Naive replay would re-fire stream deltas, sub-graph
-- dispatches, tool invocations — work that already completed and would
-- corrupt fresh state if redone. Per-plugin transforms filter the saved
-- log into a structural-history-only view: provider chats get rebuilt
-- (so the next turn lands on a populated history), the chat surface
-- repopulates `state.entries`, and every per-firing / per-graph artifact
-- gets dropped.
--
-- # Public API
--
--   resume.register(plugin_name, fn)
--     Register a transform for `plugin_name`. `fn(env)` runs once per
--     replayed envelope targeted at this plugin and returns either a
--     (possibly mutated) envelope to deliver or nil to drop. The transform
--     receives `{ type, body, from, ts }` and may mutate `body` freely
--     (the caller deep-copies before invocation). Re-registering the same
--     plugin overwrites silently — last-write-wins lets composition stay
--     simple.
--
--   resume.transform_for_plugin(plugin_name, env)
--     Apply the plugin's registered transform to `env`. If the plugin
--     hasn't registered, return nil to drop — replay is opt-in. This
--     matches the user's intent: "default behaviour: drop events for
--     plugins that haven't registered".
--
--   resume.is_active() / resume.set_active(bool)
--     Lifecycle bit driven from init.lua. `is_active()` reports whether
--     the current boot is resuming a parent session. ncp.lua consults
--     this to decide whether to apply transforms during saved_log replay.
--     init.lua flips it on when it reads a non-empty resume_target.
--
--   resume._reset()
--     Test-only: clear the registry + active flag.
--
-- # Why a Lua lib instead of a plugin
--
-- Per the user: "as we work purely with messages then it doesn't make
-- sense to make it its own plugin to add one more roundtrip". Transforms
-- live in the same Lua VM as ncp.lua's broadcast loop — applying one is
-- a function call, not an NCP envelope round-trip across stdin/stdout.

local M = {}

-- Registry: plugin_name -> transform function.
local transforms = {}

-- Whether the current boot is resuming a parent session. Default false;
-- init.lua flips on if it reads a sidechannel resume_target. ncp.lua
-- only routes saved_log entries through transforms when this is true.
local active = false

-- Register a per-plugin transform. See module doc for envelope shape.
function M.register(plugin_name, fn)
  if type(plugin_name) ~= "string" or plugin_name == "" then
    error("resume.register: plugin_name must be a non-empty string", 2)
  end
  if type(fn) ~= "function" then
    error("resume.register: fn must be a function", 2)
  end
  transforms[plugin_name] = fn
end

-- Apply the registered transform for `plugin_name` to `env`. Returns
-- the (possibly rewritten) envelope or nil to drop. No registration =
-- drop (replay is opt-in).
--
-- Errors in user transforms drop the envelope silently — a faulty
-- transform during boot replay shouldn't crash the engine. The next
-- envelope still gets a fair chance.
function M.transform_for_plugin(plugin_name, env)
  local fn = transforms[plugin_name]
  if fn == nil then return nil end
  local ok, result = pcall(fn, env)
  if not ok then return nil end
  return result
end

-- Lifecycle bit. init.lua flips on at boot when the sidechannel
-- resume_target file is present and parses cleanly.
function M.set_active(v)
  active = v and true or false
end

function M.is_active()
  return active
end

-- Test-only: reset registry + active flag.
function M._reset()
  transforms = {}
  active = false
end

-- Test-only: peek at how many transforms are registered.
function M._registered_count()
  local n = 0
  for _ in pairs(transforms) do n = n + 1 end
  return n
end

return M
