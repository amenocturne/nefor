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

-- ------------------------------------------------------------------
-- shipped-plugin defaults
-- ------------------------------------------------------------------
--
-- Per-plugin transforms for the stack `init.lua` ships by default. Kept
-- in this module rather than scattered across init.lua because:
--   (1) a Lua test harness can call `register_defaults` and then exercise
--       each transform against fixtures of past saved-log entries,
--   (2) init.lua's plugin spawn block stays scannable — one call vs
--       a dozen inline closures.
--
-- Adding a new plugin: register a transform here (or call `register`
-- directly from init.lua for project-private plugins). The default for
-- unregistered plugins is to drop everything — that's the safe default
-- for resume (no surprise replays of in-flight artefacts).

-- nefor-tui transform: replay structural chat history. The chat surface
-- (`starter/chat.lua`) handles `chat.message.append` and `chat.stream.end`
-- by appending entries to `state.entries`; replaying those rebuilds the
-- transcript verbatim. Drop everything else: `chat.stream.delta` (deltas
-- already merged into the final stream.end text — replaying would
-- duplicate), `graph.*` (sub-graph state is per-firing, doesn't carry
-- over), key/mouse events (those came from the user, not the bus), and
-- popup/toast events (transient UI).
local TUI_KEPT = {
  ["chat.message.append"] = true,
  ["chat.stream.end"]     = true,
  ["chat.session.stats"]  = true,
  ["chat.tool.start"]     = true,
  ["chat.tool.end"]       = true,
  ["chat.model.set_ack"]  = true,
  ["chat.auth.status"]    = true,
}

local function tui_transform(env)
  if env.type ~= "event" or type(env.body) ~= "table" then return nil end
  local kind = env.body.kind
  if type(kind) ~= "string" then return nil end
  if TUI_KEPT[kind] then return env end
  return nil
end

-- openai-provider transform factory (per-instance because the plugin
-- name is configurable: "ollama", "openai", etc., and the event prefix
-- mirrors that name). Replay `<name>.chat.create`, `<name>.chat.append`,
-- `<name>.chat.complete` so the provider rebuilds its `Chats` map. Drop
-- stream deltas (per-turn artefacts), `<name>.chat.complete.result`
-- (replaying would re-trigger graph.node_result emissions), errors, and
-- auth/model events (those re-fire on a fresh boot via the live path).
local function provider_transform_factory(provider_name)
  -- Pre-build the kept-prefix lookup once so the per-event hot path is
  -- a single string equality.
  local prefix = provider_name .. "."
  local kept = {
    [prefix .. "chat.create"]   = true,
    [prefix .. "chat.append"]   = true,
    [prefix .. "chat.complete"] = true,
  }
  return function(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return nil end
    local kind = env.body.kind
    if type(kind) ~= "string" then return nil end
    if kept[kind] then return env end
    return nil
  end
end

-- Convenience: register the default transforms for a stack composed of
-- nefor-tui + an openai-style provider. Other plugins (reasoner-graph,
-- tool-gate, basic-tools, mock-plugin, generic-*, nefor-combinators)
-- get no registration → default-drop, which matches the brief.
function M.register_defaults(provider_name)
  M.register("nefor-tui", tui_transform)
  if type(provider_name) == "string" and #provider_name > 0 then
    M.register(provider_name, provider_transform_factory(provider_name))
  end
end

-- Exposed for tests so they can verify factory output without going
-- through register/transform_for_plugin.
M._tui_transform = tui_transform
M._provider_transform_factory = provider_transform_factory

return M
