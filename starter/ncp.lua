-- starter/ncp.lua — NCP v0.1 protocol implementation (Lua).
--
-- Public API:
--   ncp.step(saved_log, current_log) -- called from the global step hook
--   ncp.spawn(cfg)                   -- spawn a plugin with optional transforms
--
-- The engine is a pure string-layer event bus. Every inbound line gets
-- appended to `current_log` and `ncp.step` is invoked. This module inspects
-- the new tail entry and implements:
--   * `ready` / `ready_ok` handshake (with `protocol_version` check).
--   * Broadcast of `type:"event"` messages to every *other* ready plugin.
--   * Replay-on-attach: when a plugin readies, resend the current session's
--     prior event messages to it so it observes the same bus history as
--     plugins that were already connected.
--   * `error` emission on malformed inbound (unparseable JSON, bad envelope,
--     unknown kind, version mismatch, invalid ready).
--   * Per-plugin envelope transforms: `from_plugin` runs at ingress (after
--     a plugin emits, before broadcast); `to_plugin` runs at egress, per
--     target. Both are optional, default identity, and may return `nil` to
--     drop the envelope. Lets the user's init.lua adapt vendor namespaces
--     (e.g. `cc.*` → `chat.*`) without modifying plugins.
--
-- State lives in module-level locals and is reset on module reload. Nothing
-- here is shared across step invocations except the `ready_plugins` set and
-- the `plugin_transforms` table; the engine holds the authoritative event log.
--
-- # Tradeoffs documented in code
--
-- * Replay re-stamps `ts`. The engine stamps `ts = now` on every outbound
--   `nefor.engine.send`, so replayed events arrive with a fresh timestamp
--   rather than the original. A future engine binding could accept a
--   `ts_override` and preserve wire-level ordering; out of scope for v1.
-- * Broadcast costs N-1 targeted sends. NCP broadcast excludes the sender;
--   the engine's broadcast includes every connected plugin. Lua bridges the
--   gap with `nefor.engine.plugins()` + per-peer `send`.
-- * `saved_log` (parent-session hydration) is deliberately ignored in v1.
--   Automatic replay of a prior session's events to a fresh plugin set
--   would double-count or confuse plugins that aren't prepared for
--   rewound history. Session resumption semantics are deferred (see
--   nefor-reasoner-architecture.md: D-21a-deferred).

local json = require("lib.json")

local M = {}

-- Protocol constants.
local NCP_VERSION = "0.1"
local ENGINE_VERSION = "0.1.0"

-- Ready plugins, keyed by plugin name. Value = index into current_log at
-- the moment the handshake was accepted, so replay-on-attach can slice
-- strictly-prior entries without counting messages that arrived after the
-- ready (which the engine delivers via the normal broadcast path).
local ready_plugins = {}

-- Per-plugin envelope transforms, keyed by plugin name.
-- Each entry: { from_plugin = function|nil, to_plugin = function|nil }.
-- `from_plugin(env)` runs once per emission at ingress; `to_plugin(env)` runs
-- per peer at egress. Both receive `{type, body, from}` (and `ts` on
-- to_plugin) and return either a (possibly mutated) envelope table or nil
-- to drop. Errors are caught — a faulty transform never crashes step.
local plugin_transforms = {}

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

-- Try to decode a JSON string. Returns (decoded, nil) on success or
-- (nil, err_message) on failure. Wraps pcall so a bad line is a protocol
-- fault we report, not an uncaught error that takes down step.
local function try_decode(s)
  local ok, v = pcall(json.decode, s)
  if not ok then return nil, tostring(v) end
  return v, nil
end

local function encode(v)
  return json.encode(v)
end

-- Recursively copy a value. Used at egress so each peer's `to_plugin`
-- transform sees its own envelope — without this, mutating `env.body` in
-- one peer's transform would leak to subsequent peers in the broadcast
-- fan-out. JSON-shaped values are safe to deep-copy with this naive walk
-- (no metatables, no cycles).
local function deep_copy(v)
  if type(v) ~= "table" then return v end
  local out = {}
  for k, vv in pairs(v) do
    out[k] = deep_copy(vv)
  end
  return out
end

-- Build an engine-originated wire envelope. Per NCP §3 engine-broadcast
-- envelopes carry `from:"engine"` + engine-stamped `ts` in addition to the
-- plugin-authored `type` + `body`. `nefor.engine.now()` returns the
-- authoritative ISO-8601 timestamp.
local function engine_envelope(body_tbl, kind)
  return {
    type = kind,
    from = "engine",
    ts   = nefor.engine.now(),
    body = body_tbl,
  }
end

local function emit_ready_ok(target)
  nefor.engine.send(encode(engine_envelope({
    kind = "ready_ok",
    engine_version = ENGINE_VERSION,
  }, "system")), target)
end

local function emit_error(target, code, message)
  nefor.engine.send(encode(engine_envelope({
    kind = "error",
    code = code,
    message = message,
  }, "system")), target)
end

-- List of currently connected plugin names, minus `exclude`. Used for
-- broadcast-minus-sender.
local function peers_minus(exclude)
  local out = {}
  for _, name in ipairs(nefor.engine.plugins()) do
    if name ~= exclude then
      out[#out + 1] = name
    end
  end
  return out
end

-- ------------------------------------------------------------------
-- system message handling
-- ------------------------------------------------------------------

-- Forward declarations: handle_system, handle_ready, and replay_prior_events
-- reference each other below; Lua resolves local names lexically so we need
-- the `local` declaration to precede every use site.
local handle_system
local handle_ready
local replay_prior_events

-- Handle a received `system` message from `origin`. `body` is the already-
-- parsed body table (or may be nil/non-table on malformed input).
handle_system = function(origin, body, current_log, tail_index)
  if type(body) ~= "table" or type(body.kind) ~= "string" then
    emit_error(origin, "malformed_envelope", "system body missing 'kind'")
    return
  end

  if body.kind == "ready" then
    handle_ready(origin, body, current_log, tail_index)
    return
  end

  -- Plugins only legitimately send `ready` as a system kind (per §5).
  -- Anything else is a protocol fault on their side.
  emit_error(origin, "unknown_kind",
    "plugins may only send 'ready' as a system kind; got '" .. body.kind .. "'")
end

-- Dispatch for `ready` messages. Split out for clarity.
handle_ready = function(origin, body, current_log, tail_index)
  -- Structural check first (missing field, wrong type) — that's
  -- `invalid_ready`. Version-check next — that's `protocol_version_mismatch`.
  if type(body.protocol_version) ~= "string" then
    emit_error(origin, "invalid_ready",
      "ready body missing required string field 'protocol_version'")
    return
  end
  if body.protocol_version ~= NCP_VERSION then
    emit_error(origin, "protocol_version_mismatch",
      "engine speaks NCP " .. NCP_VERSION ..
      "; plugin declared '" .. body.protocol_version .. "'")
    return
  end

  -- Policy: re-ready from an already-ready plugin is a protocol fault. The
  -- spec doesn't explicitly name the case, but `ready` is defined as "first
  -- message a plugin sends after connecting" (§5.1) — a second ready is not
  -- a first. We surface `invalid_ready` and ignore the repeat rather than
  -- duplicate the replay burst.
  if ready_plugins[origin] then
    emit_error(origin, "invalid_ready",
      "plugin already ready; 'ready' is only valid as the first message")
    return
  end

  ready_plugins[origin] = tail_index
  emit_ready_ok(origin)
  replay_prior_events(origin, current_log, tail_index)
end

-- Apply the source plugin's `from_plugin` transform (if any) to a decoded
-- envelope. Returns the (possibly rewritten) envelope, or nil to drop.
-- Errors in user code surface as `transform_error` to the source plugin
-- and the envelope is dropped — better than crashing step.
local function apply_from_plugin(origin, env)
  local t = plugin_transforms[origin]
  if not t or not t.from_plugin then return env end
  local ok, result = pcall(t.from_plugin, env)
  if not ok then
    emit_error(origin, "transform_error",
      "from_plugin transform raised: " .. tostring(result))
    return nil
  end
  return result
end

-- Apply the target plugin's `to_plugin` transform (if any) to a wire
-- envelope. Returns the (possibly rewritten) envelope, or nil to drop for
-- this peer. Errors drop the envelope silently for the target — the target
-- didn't cause them and shouldn't see a protocol error.
local function apply_to_plugin(target, env)
  local t = plugin_transforms[target]
  if not t or not t.to_plugin then return env end
  local ok, result = pcall(t.to_plugin, env)
  if not ok then return nil end
  return result
end

-- Decode a plugin-authored payload and run it through `from_plugin`.
-- Returns the post-transform envelope `{type, body, from}`, or nil if the
-- payload doesn't parse, isn't an object, or the transform dropped it.
local function decode_and_apply_from(origin, payload)
  local decoded = select(1, try_decode(payload))
  if not decoded or type(decoded) ~= "table" then return nil end
  return apply_from_plugin(origin, {
    type = decoded.type,
    body = decoded.body,
    from = origin,
  })
end

-- Stamp + apply `to_plugin` + send. `env_in` is the post-from_plugin
-- envelope `{type, body, from}`; we add an authoritative `ts` here per §3.
-- Each peer gets its own `ts` to keep the broadcast loop simple; preserving
-- a single shared `ts` across the fan-out is a v2 concern (see module doc).
local function send_to_peer(target, env_in)
  -- Body is deep-copied so peers can't see each other's `to_plugin`
  -- mutations. type/from/ts are scalars and are reassigned per peer
  -- anyway, so they don't need copying.
  local env = apply_to_plugin(target, {
    type = env_in.type,
    from = env_in.from,
    ts   = nefor.engine.now(),
    body = deep_copy(env_in.body),
  })
  if env == nil then return end
  nefor.engine.send(encode(env), target)
end

-- Replay every plugin-originated `type:"event"` entry seen before the
-- handshake. Replayed envelopes pass through `from_plugin` (at the source)
-- and `to_plugin` (at the new attacher), so a late attacher sees the same
-- transformed view as if it had been there all along. The engine stamps a
-- fresh `ts` on each outbound send — see module-level tradeoffs. Order is
-- preserved by iterating current_log in slice order.
replay_prior_events = function(target, current_log, tail_index)
  for i = 1, tail_index - 1 do
    local entry = current_log[i]
    -- Skip Step-originated entries: those are the engine's own forwarding
    -- fan-out of prior events, not originals. Replaying them would
    -- double-deliver.
    if entry.origin ~= "step" then
      local env_in = decode_and_apply_from(entry.origin, entry.payload)
      if env_in and env_in.type == "event" then
        send_to_peer(target, env_in)
      end
    end
  end
end

-- ------------------------------------------------------------------
-- event broadcast
-- ------------------------------------------------------------------

local function handle_event(origin, payload)
  -- Drop events from plugins that haven't readied yet — the spec's ready
  -- timeout (§2) combined with "ready is the first message" (§5.1) means a
  -- well-behaved plugin sends nothing else first. We emit a malformed-
  -- envelope error to nudge the implementer; the connection stays open so
  -- the plugin can still ready up.
  if not ready_plugins[origin] then
    emit_error(origin, "malformed_envelope",
      "received event before 'ready' handshake completed")
    return
  end

  local env_in = decode_and_apply_from(origin, payload)
  if env_in == nil then return end

  for _, peer in ipairs(peers_minus(origin)) do
    if ready_plugins[peer] then
      send_to_peer(peer, env_in)
    end
  end
end

-- ------------------------------------------------------------------
-- public entry point
-- ------------------------------------------------------------------

function M.step(_saved_log, current_log)
  -- Note on `_saved_log`: parent-session hydration is deferred (see module
  -- doc). Accepted and ignored so the engine-to-Lua contract stays stable.

  local tail_index = #current_log
  if tail_index == 0 then return end

  local entry = current_log[tail_index]
  -- Only react to lines the engine received from a plugin. Entries with
  -- origin == "step" are this module's own outbound sends — reprocessing
  -- them would infinite-loop on a malformed reply.
  if entry.origin == "step" then return end

  local decoded, decode_err = try_decode(entry.payload)
  if decode_err ~= nil then
    emit_error(entry.origin, "malformed_envelope",
      "payload is not valid JSON: " .. decode_err)
    return
  end
  if type(decoded) ~= "table" then
    emit_error(entry.origin, "malformed_envelope",
      "payload is not a JSON object")
    return
  end

  local t = decoded.type
  if t == "system" then
    handle_system(entry.origin, decoded.body, current_log, tail_index)
  elseif t == "event" then
    -- §3: body must be a JSON object even for events. Enforce here rather
    -- than trust the plugin.
    if type(decoded.body) ~= "table" then
      emit_error(entry.origin, "body_not_object",
        "event body must be a JSON object")
      return
    end
    handle_event(entry.origin, entry.payload)
  else
    emit_error(entry.origin, "malformed_envelope",
      "envelope 'type' must be 'system' or 'event'")
  end
end

-- ------------------------------------------------------------------
-- public spawn API: nefor.plugins.spawn + transform registration
-- ------------------------------------------------------------------

-- ncp.spawn — wraps `nefor.plugins.spawn` to also accept optional
-- `from_plugin` / `to_plugin` envelope transforms. The engine's spawn API
-- rejects unknown fields (deliberately — it's part of the bus, not the
-- protocol), so transforms live here in the protocol layer instead.
--
-- Example:
--   ncp.spawn {
--     name    = "mock-plugin",
--     command = { "../target/debug/mock-plugin" },
--     from_plugin = function(env)
--       -- env = { type = "event"|"system", body = {...}, from = "mock-plugin" }
--       if env.body and env.body.kind == "cc.stream.end" then
--         env.body.kind = "chat.stream.end"
--       end
--       return env  -- or nil to drop the envelope
--     end,
--   }
function M.spawn(cfg)
  if type(cfg) ~= "table" then
    error("ncp.spawn: expected table config, got " .. type(cfg), 2)
  end
  if type(cfg.name) ~= "string" or cfg.name == "" then
    error("ncp.spawn: 'name' is required (non-empty string)", 2)
  end

  local from_plugin = cfg.from_plugin
  local to_plugin   = cfg.to_plugin
  if from_plugin ~= nil and type(from_plugin) ~= "function" then
    error("ncp.spawn: 'from_plugin' must be a function or nil", 2)
  end
  if to_plugin ~= nil and type(to_plugin) ~= "function" then
    error("ncp.spawn: 'to_plugin' must be a function or nil", 2)
  end
  if from_plugin or to_plugin then
    plugin_transforms[cfg.name] = {
      from_plugin = from_plugin,
      to_plugin   = to_plugin,
    }
  end

  -- Forward to the engine's spawn API with transforms stripped. The engine
  -- rejects unknown fields, so we hand it only the fields it knows.
  nefor.plugins.spawn({
    name    = cfg.name,
    command = cfg.command,
  })
end

-- Exposed for tests only. Resets module state between scenarios so each
-- test starts from a clean slate.
function M._reset()
  ready_plugins = {}
  plugin_transforms = {}
end

-- Exposed for tests only. Registers a transform for `name` without going
-- through `M.spawn` (which calls into the real engine spawn API). Lets the
-- ncp_test.lua suite exercise transforms without mocking `nefor.plugins`.
function M._test_set_transforms(name, transforms)
  plugin_transforms[name] = transforms
end

return M
