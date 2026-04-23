-- starter/ncp.lua — NCP v0.1 protocol implementation (Lua).
--
-- Public API:
--   ncp.step(saved_log, current_log) -- called from the global step hook
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
--
-- State lives in module-level locals and is reset on module reload. Nothing
-- here is shared across step invocations except the `ready_plugins` set; the
-- engine holds the authoritative event log.
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

-- Re-wrap a plugin-authored event payload as a fully-stamped envelope
-- addressed at a peer. Plugins send `{type, body}`; receivers need
-- `{type, from, ts, body}` (§3). Starter's NCP layer is the only thing that
-- can stamp authoritative `from`/`ts` on the wire, so we do it here.
--
-- Returns the JSON line ready for `nefor.engine.send`, or nil if the
-- payload doesn't parse (caller has already logged/ignored).
local function forward_envelope(sender, payload)
  local decoded = select(1, try_decode(payload))
  if not decoded or type(decoded) ~= "table" then return nil end
  return encode({
    type = decoded.type,
    from = sender,
    ts   = nefor.engine.now(),
    body = decoded.body,
  })
end

-- Replay every plugin-originated `type:"event"` entry seen before the
-- handshake. The engine stamps a fresh `ts` on each outbound send — see
-- module-level tradeoffs. Order is preserved by iterating current_log in
-- slice order.
replay_prior_events = function(target, current_log, tail_index)
  for i = 1, tail_index - 1 do
    local entry = current_log[i]
    -- Skip Step-originated entries: those are the engine's own forwarding
    -- fan-out of prior events, not originals. Replaying them would
    -- double-deliver.
    if entry.origin ~= "step" then
      local decoded = select(1, try_decode(entry.payload))
      if decoded and decoded.type == "event" then
        local wire = forward_envelope(entry.origin, entry.payload)
        if wire then nefor.engine.send(wire, target) end
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

  local wire = forward_envelope(origin, payload)
  if wire == nil then return end
  for _, peer in ipairs(peers_minus(origin)) do
    if ready_plugins[peer] then
      nefor.engine.send(wire, peer)
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

-- Exposed for tests only. Resets module state between scenarios so each
-- test starts from a clean slate.
function M._reset()
  ready_plugins = {}
end

return M
