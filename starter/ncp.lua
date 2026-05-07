-- starter/ncp.lua — NCP v0.1 protocol implementation (Lua).
--
-- ## Public API
--
--   ncp.invoke_from_plugin(source, payload)  -- broker hook for inbound lines
--   ncp.dispatch(current_log)                -- broker hook after each emission
--   ncp.spawn(cfg)                           -- register a wrapper (transforms)
--
-- ## Architecture (post wrapper-callback refactor)
--
-- Wrappers' `from_plugin` and `to_plugin` are **batched side-effecting
-- callbacks**, not transforms. The framework hands a LIST of parsed
-- envelopes (`envs`) to the wrapper per dispatch tick; the wrapper iterates
-- the list and decides what (if anything) flows onto the bus or down to a
-- peer's stdin. The framework never inspects the return value.
--
-- Single-envelope is just a one-element list — wrappers that don't care
-- about batching write `for _, env in ipairs(envs) do ... end` and the
-- semantics match the per-envelope contract that came before. Wrappers
-- that benefit from batching (TUI re-rendering on resume, provider
-- history rebuild) read across the batch and amortise work.
--
-- Two outbound paths into the broker, deliberately distinct:
--
-- * `nefor.engine.send(payload, target?)` — **emission**. Appends a Step
--   entry to the bus log; the broker's drain fires `dispatch` for the new
--   tail, which iterates wrappers and calls each `to_plugin(env)`.
--   `target` is informational on the log entry (the default `to_plugin`
--   uses it to deliver only to that peer).
-- * `nefor.engine.deliver(peer, payload)` — **delivery**. Writes one line
--   to one peer's stdin without logging. Used inside `to_plugin` to push
--   the event to the wrapped Rust binary (or by any callback that needs a
--   targeted side-effect that should NOT show up on the bus).
--
-- The bus log holds **only what was explicitly published via `send`** —
-- symmetric for plugin emissions and Lua emissions. Plugin-emitted lines
-- the wrapper doesn't republish never appear on the bus.
--
-- ## Plugin-line flow (inbound)
--
-- Broker receives `payload` from plugin P:
--   1. Broker calls `M.invoke_from_plugin(P, payload)`.
--   2. We decode payload as JSON. Bad JSON → `error` reply via deliver
--      (targeted at P; doesn't pollute bus log).
--   3. `type:"system"` envelopes (ready handshake) are handled
--      framework-level here: they synthesize ready_ok / replay etc. and
--      do NOT reach the wrapper's `from_plugin`. NCP framework owns the
--      handshake.
--   4. `type:"event"` envelopes go to `plugin_transforms[P].from_plugin`,
--      called as a batched side-effecting callback with a one-element
--      `envs` list (the broker hands us one line per call). Default (no
--      `from_plugin` registered) publishes the envelope verbatim via
--      `engine.send` so the bus sees the plugin's emission.
--
-- ## Bus-event flow (outbound, every wrapper)
--
-- Broker drains new tail entries through `M.dispatch(current_log)`:
--   1. Skip Plugin entries — those are engine-injected synthetics
--      (`engine.plugin_failed`); they go through their own translation
--      below.
--   2. Step entries are bus emissions. We decode the payload and
--      accumulate per-peer batches; once the entire new-tail slice has
--      been walked, each registered wrapper's `to_plugin(envs)` fires
--      ONCE per dispatch tick with all envelopes destined for that peer
--      this tick. Default callback iterates `envs` and delivers each
--      envelope verbatim to the wrapper's peer (skipping self-emissions
--      and respecting log-entry `target` if set).
--   3. Engine entries (synthetic `engine.*`) go through the engine
--      translator (`handle_engine_envelope`) which decides whether to
--      publish chat-popup events.
--
-- Wrappers that want bespoke routing override `to_plugin`. Wrappers that
-- speak the canonical bus shape can omit it entirely and let the default
-- pass through.
--
-- ## Replay window
--
-- The legacy `in_replay` gate lived here as a framework-level skip.
-- Post-refactor it's a per-wrapper concern: each wrapper that wants to
-- skip envelopes during a replay-window reads `env.replay` on each
-- envelope inside its `to_plugin(envs)` loop. The framework walks the
-- new tail in order and stamps each Step envelope with the current
-- replay-window state (toggled inline as it hits
-- `sessions.replay.start` / `sessions.replay.end` framing markers), so
-- every envelope inside a replay window carries `env.replay = true` and
-- every live envelope carries `env.replay = false`. The global
-- `lib.replay_window.active()` flag stays in sync as a back-compat
-- channel for `nefor.bus.on_event` subscribers; wrappers should prefer
-- `env.replay` because batched dispatch removes the per-envelope
-- temporal coupling that the global flag implicitly relied on.
--
-- ## Why `from_plugin` is no longer a transform
--
-- Pre-refactor, `from_plugin` returned the envelope and dispatch owned
-- the broadcast fan-out + the auto-log of plugin emissions. That coupled
-- two responsibilities to one callback signature: "translate the shape"
-- AND "decide whether to publish". The split — wrapper owns *publishing*,
-- framework owns nothing more than the system handshake — makes
-- per-wrapper decisions explicit (call send or don't) and removes the
-- "wait, is this on the log or not?" ambiguity for replay/persistence.

local json = nefor.json
local replay_window = require("lib.replay_window")

local M = {}

-- Protocol constants.
local NCP_VERSION    = "0.1"
local ENGINE_VERSION = "0.1.0"

-- Ready plugins, keyed by plugin name. Value = monotonic id; structurally
-- a presence set, but the previous tail-index gate is gone (the bus log
-- now only carries what was published, so replay-on-attach uses the log
-- itself rather than a "strictly prior" filter).
local ready_plugins = {}

-- Per-plugin envelope callbacks, keyed by plugin name.
--   { from_plugin = function|nil, to_plugin = function|nil }
local plugin_transforms = {}

-- FIFO queue of `chat.popup` envelope tables awaiting nefor-tui's ready.
local pending_chat_popups = {}

-- High-water mark for `M.dispatch`: the highest index of `current_log`
-- already processed. The broker grows the same persistent log table on
-- each invocation; without this, `dispatch` would re-fire to_plugin for
-- entries it already handled when called multiple times in a single
-- session (e.g. once per inbound line, repeatedly cascading through
-- `drain_pending_dispatch`).
local dispatch_hwm = 0

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function try_decode(s)
  local ok, v = pcall(json.decode, s)
  if not ok then return nil, tostring(v) end
  return v, nil
end

local function encode(v)
  return json.encode(v)
end

-- Recursive deep-copy for envelopes shared across multiple wrappers'
-- to_plugin callbacks. Without this a wrapper that mutates `env.body`
-- in place leaks the mutation into every subsequent wrapper's view —
-- the dispatch loop calls each wrapper with the same envelope table.
-- JSON-shaped values are safe to deep-copy with this naive walk (no
-- metatables, no cycles).
local function deep_copy(v)
  if type(v) ~= "table" then return v end
  local out = {}
  for k, vv in pairs(v) do
    out[k] = deep_copy(vv)
  end
  return out
end

local function engine_envelope(body_tbl, kind)
  return {
    type = kind,
    from = "engine",
    ts   = nefor.engine.now(),
    body = body_tbl,
  }
end

-- Targeted error reply via `deliver` — no log entry, no bus traffic for
-- a peer's protocol fault. The peer sees the error on its stdin; the
-- rest of the bus is unaffected.
local function deliver_error(target, code, message)
  local payload = encode(engine_envelope({
    kind    = "error",
    code    = code,
    message = message,
  }, "system"))
  pcall(nefor.engine.deliver, target, payload)
end

local function deliver_ready_ok(target)
  local payload = encode(engine_envelope({
    kind           = "ready_ok",
    engine_version = ENGINE_VERSION,
  }, "system"))
  pcall(nefor.engine.deliver, target, payload)
end

-- ------------------------------------------------------------------
-- system message handling (NCP handshake — framework-level)
-- ------------------------------------------------------------------

local replay_prior_events  -- forward decl

local function handle_ready(origin, body, current_log)
  if type(body.protocol_version) ~= "string" then
    deliver_error(origin, "invalid_ready",
      "ready body missing required string field 'protocol_version'")
    return
  end
  if body.protocol_version ~= NCP_VERSION then
    deliver_error(origin, "protocol_version_mismatch",
      "engine speaks NCP " .. NCP_VERSION ..
      "; plugin declared '" .. body.protocol_version .. "'")
    return
  end
  if ready_plugins[origin] then
    deliver_error(origin, "invalid_ready",
      "plugin already ready; 'ready' is only valid as the first message")
    return
  end

  ready_plugins[origin] = true
  deliver_ready_ok(origin)
  replay_prior_events(origin, current_log)

  -- Flush any popups buffered while nefor-tui was still booting.
  if origin == "nefor-tui" and #pending_chat_popups > 0 then
    for _, popup in ipairs(pending_chat_popups) do
      pcall(nefor.engine.deliver, "nefor-tui", encode(popup))
    end
    pending_chat_popups = {}
  end
end

local function handle_system(origin, body, current_log)
  if type(body) ~= "table" or type(body.kind) ~= "string" then
    deliver_error(origin, "malformed_envelope", "system body missing 'kind'")
    return
  end
  if body.kind == "ready" then
    handle_ready(origin, body, current_log)
    return
  end
  deliver_error(origin, "unknown_kind",
    "plugins may only send 'ready' as a system kind; got '" .. body.kind .. "'")
end

-- ------------------------------------------------------------------
-- replay-on-attach
-- ------------------------------------------------------------------
--
-- When a plugin readies after others have already been emitting, it
-- needs to see the bus events it missed. We re-deliver the prior bus
-- log to the new attacher by walking it and accumulating a single envs
-- list, then calling its wrapper's to_plugin (if any) ONCE with the
-- whole list, exactly as if the entries were dispatched together in a
-- single tick.
--
-- We DON'T republish via `send` (that would re-fire every wrapper's
-- to_plugin again, doubling traffic). We invoke the new attacher's
-- to_plugin directly with the parsed envelopes.

-- Default to_plugin path for a single envelope. Used both as the
-- per-envelope inner step of the framework default (when a wrapper has
-- no `to_plugin` override) and by no-override peers iterating their own
-- batches via the framework default.
local function default_deliver_for(target, env, entry_target)
  -- Default to_plugin: deliver the envelope verbatim to `target`,
  -- subject to:
  --   * skip if env.from == target (don't echo a peer's emission back
  --     to itself)
  --   * skip if entry_target is set and != target (a `send(payload, X)`
  --     was addressed at X; other peers shouldn't see it)
  --   * skip if kind starts with "<peer>." and <peer> != target —
  --     legacy peer-prefixed routing convention
  if env.from == target then return end
  if entry_target ~= nil and entry_target ~= target then return end

  -- Legacy peer-prefix routing: kinds shaped "<peer>.<rest>" addressed
  -- at one specific peer (other than the sender) deliver only to that
  -- peer. We only apply this when the prefix actually matches a ready
  -- peer (avoids false positives on generic kinds like "test.ping" or
  -- "graph.node.fired" whose first component is not a peer name).
  local k = env.body and env.body.kind
  if type(k) == "string" then
    local prefix = k:match("^([^.]+)%.")
    if prefix and prefix ~= env.from and ready_plugins[prefix] then
      if prefix ~= target then return end
    end
  end

  -- Strip framework-only fields (`replay`, …) when encoding for the
  -- wire; the protocol parser rejects unknown envelope fields.
  pcall(nefor.engine.deliver, target, encode({
    type = env.type,
    from = env.from,
    ts   = env.ts,
    body = env.body,
  }))
end

-- Fan a per-peer batch of envelopes (already deep-copied for this
-- peer's view) into the wrapper's `to_plugin(envs)` callback, or
-- default-deliver each envelope when no override is registered.
--
-- `entry_targets` is a parallel list of the source log entry's `target`
-- field (nil for broadcast, plugin name for targeted) — the default
-- delivery path consults it to filter peers other than the addressee.
-- Wrapper overrides don't see entry_target directly; if a wrapper needs
-- targeting, it inspects env.body.kind / env.from and delivers itself.
local function call_to_plugin_for_batch(target, envs, entry_targets)
  if #envs == 0 then return end
  local t = plugin_transforms[target]
  local cb = t and t.to_plugin
  if cb then
    local ok, err = pcall(cb, envs)
    if not ok then
      nefor.log.warn("ncp: to_plugin raised; dropping batch for peer", {
        peer  = target,
        count = #envs,
        error = tostring(err),
      })
    end
    return
  end

  for i, env in ipairs(envs) do
    default_deliver_for(target, env, entry_targets[i])
  end
end

-- Replay prior bus events to a freshly-readied peer. Walks the current
-- log up to the entry that triggered this ready (exclusive), filters to
-- bus emissions (Step entries), decodes each, accumulates a single envs
-- list (one entry per replayed envelope), and fires the new attacher's
-- to_plugin once with the whole batch.
--
-- Replay-on-attach happens outside the live `sessions.replay.*` framing
-- (the new attacher is just catching up to "what's already on the bus";
-- it isn't a session-replay event), so every envelope's `replay` flag
-- is whatever the global replay_window state says at this moment —
-- normally false, but if a session replay is in progress the new peer
-- sees the same view as everyone else.
replay_prior_events = function(target, current_log)
  local current_replay = replay_window.active()
  local envs = {}
  local entry_targets = {}
  for _, entry in ipairs(current_log) do
    if entry.origin == "step" then
      -- Only re-deliver to this peer if the original emission was a
      -- broadcast (target nil) or addressed at this peer. Targeted
      -- emissions for other peers are not for this attacher.
      if entry.target == nil or entry.target == target then
        local decoded = select(1, try_decode(entry.payload))
        if type(decoded) == "table" and decoded.type == "event"
            and type(decoded.body) == "table" then
          local from = (type(decoded.from) == "string") and decoded.from or "engine"
          envs[#envs + 1] = {
            type   = decoded.type,
            from   = from,
            ts     = decoded.ts,
            body   = deep_copy(decoded.body),
            replay = current_replay,
          }
          entry_targets[#entry_targets + 1] = entry.target
        end
      end
    end
  end
  call_to_plugin_for_batch(target, envs, entry_targets)
end

-- ------------------------------------------------------------------
-- engine envelopes (synthetic engine.*)
-- ------------------------------------------------------------------

local function handle_engine_envelope(decoded)
  local body = decoded.body
  if type(body) ~= "table" or type(body.kind) ~= "string" then return end

  if body.kind == "engine.plugin_failed" then
    local chat_present = false
    for _, name in ipairs(nefor.engine.plugins()) do
      if name == "nefor-tui" then chat_present = true; break end
    end
    if not chat_present then return end

    local plugin = tostring(body.plugin or "<unknown>")
    local phase  = tostring(body.phase  or "<unknown>")
    local reason = tostring(body.reason or "<no reason>")
    local popup = engine_envelope({
      kind    = "chat.popup",
      level   = "error",
      title   = "plugin failed",
      message = string.format("%s failed during %s: %s", plugin, phase, reason),
      source  = "engine",
    }, "event")

    if not ready_plugins["nefor-tui"] then
      pending_chat_popups[#pending_chat_popups + 1] = popup
      return
    end
    -- Use deliver: the popup is targeted (only nefor-tui needs to see
    -- it) and going through send would inflate the bus log with one
    -- step entry per engine.plugin_failed.
    pcall(nefor.engine.deliver, "nefor-tui", encode(popup))
  end
end

-- ------------------------------------------------------------------
-- public entry point: invoke_from_plugin (broker → Lua, inbound lines)
-- ------------------------------------------------------------------

function M.invoke_from_plugin(source, raw_payload)
  if type(source) ~= "string" or source == "" then return end
  if type(raw_payload) ~= "string" then return end

  local decoded, decode_err = try_decode(raw_payload)
  if decode_err ~= nil then
    deliver_error(source, "malformed_envelope",
      "payload is not valid JSON: " .. decode_err)
    return
  end
  if type(decoded) ~= "table" then
    deliver_error(source, "malformed_envelope",
      "payload is not a JSON object")
    return
  end

  local t = decoded.type
  if t == "system" then
    -- For replay-on-attach, the framework needs the current bus log to
    -- walk it. The Lua side reads it back via the dispatch hook's
    -- argument; from this entry-point we only have the broker-supplied
    -- payload. Use `_current_log_ref` (set by `M.dispatch` on the most
    -- recent invocation) — it's a stable ref in the Lua VM.
    local cl = M._current_log_ref or {}
    handle_system(source, decoded.body, cl)
    return
  end

  if t == "event" then
    if type(decoded.body) ~= "table" then
      deliver_error(source, "body_not_object",
        "event body must be a JSON object")
      return
    end
    -- Drop events from non-ready plugins.
    if not ready_plugins[source] then
      deliver_error(source, "malformed_envelope",
        "received event before 'ready' handshake completed")
      return
    end

    local from = (type(decoded.from) == "string") and decoded.from or source
    local env = {
      type = decoded.type,
      from = from,
      body = decoded.body,
    }

    local hook = plugin_transforms[source] and plugin_transforms[source].from_plugin
    if hook then
      -- Batched contract: hook receives a list of envelopes per
      -- invocation. The broker hands us one inbound line at a time, so
      -- this list is single-element; wrappers iterate the list the same
      -- way they would for a multi-element batch.
      local ok, err = pcall(hook, { env })
      if not ok then
        deliver_error(source, "transform_error",
          "from_plugin callback raised: " .. tostring(err))
      end
      return
    end

    -- Default callback: publish the envelope verbatim onto the bus via
    -- `send` (broadcast). Wrappers without an explicit `from_plugin`
    -- behave as identity passthrough — same effective behavior as the
    -- pre-refactor "no from_plugin transform" path.
    local payload = encode({
      type = "event",
      from = from,
      ts   = nefor.engine.now(),
      body = decoded.body,
    })
    nefor.engine.send(payload)
    return
  end

  deliver_error(source, "malformed_envelope",
    "envelope 'type' must be 'system' or 'event'")
end

-- ------------------------------------------------------------------
-- public entry point: dispatch (broker → Lua, every new bus entry)
-- ------------------------------------------------------------------

-- Dispatch processes new entries only. The broker passes the same
-- current_log table on every call (it grows in place); re-calls without
-- growth shouldn't re-fire to_plugin for entries already handled. We
-- track a high-water mark per-log via a hidden field.
--
-- Batched fan-out: the new tail can hold N step entries in one drain
-- tick (cascade from a wrapper's own `engine.send`, replay burst from
-- sessions, etc.). We accumulate per-peer envs lists across the whole
-- tail, then fire each ready wrapper's `to_plugin(envs)` ONCE with its
-- batch. Engine-origin entries (synthetic engine.*) interleave; they
-- run through `handle_engine_envelope` in their original position,
-- which is fine because the engine translator only mutates pending
-- popups / queues new bus emissions and doesn't itself drive
-- `to_plugin` for the same entry.
function M.dispatch(current_log)
  -- Stash the current_log ref so `invoke_from_plugin` (which the broker
  -- calls in a different code path) can use it for replay-on-attach.
  M._current_log_ref = current_log

  local tail_index = #current_log
  if tail_index == 0 then return end

  if tail_index <= dispatch_hwm then return end

  -- Snapshot the ready peer list once per dispatch tick. The result
  -- shape is:
  --   per_peer_envs[name]    = { env1, env2, ... }   (deep-copied bodies)
  --   per_peer_targets[name] = { entry.target1, ... }  (parallel array)
  local plugins_snapshot = nefor.engine.plugins()
  local per_peer_envs = {}
  local per_peer_targets = {}
  for _, name in ipairs(plugins_snapshot) do
    if ready_plugins[name] then
      per_peer_envs[name] = {}
      per_peer_targets[name] = {}
    end
  end

  -- Replay-window framing — toggle the global flag inline as we walk
  -- the tail and stamp each Step env with the window state at the
  -- moment of its position in the batch. The Rust drain loop runs the
  -- entire batch's `to_plugin` calls before any `dispatch_subscriptions`
  -- handler fires (vm.rs `drain_pending_dispatch`), so a bus.on_event
  -- subscriber alone can't gate replayed envelopes riding in the same
  -- batch as the `replay.start` marker (Bug 5). Stamping per-envelope
  -- means a wrapper iterating its envs list reads `env.replay` directly
  -- and the per-envelope state is preserved across the batch boundary,
  -- which the global `replay_window.active()` flag CANNOT do at
  -- end-of-tick when called from inside a wrapper's loop — by then the
  -- flag is whatever the LAST framing marker set it to.
  --
  -- The global flag is still kept in sync because back-compat
  -- subscribers (sessions's persistence guard, defense-in-depth bus
  -- handlers) read it. Wrappers should prefer `env.replay`.
  --
  -- nefor-tui deliberately does NOT consult `env.replay` — the TUI
  -- surface NEEDS replayed envelopes to repaint the transcript on
  -- resume. Other wrappers (tool-gate, openai-provider, …) skip on
  -- `env.replay` because their peer plugins would treat replayed
  -- envelopes as fresh invocations and produce duplicate side effects.
  for i = dispatch_hwm + 1, tail_index do
    local entry = current_log[i]
    if entry.origin == "engine" then
      local decoded, decode_err = try_decode(entry.payload)
      if decode_err == nil and type(decoded) == "table" then
        handle_engine_envelope(decoded)
      end
    elseif entry.origin == "step" then
      local decoded = select(1, try_decode(entry.payload))
      if type(decoded) == "table" and decoded.type == "event"
          and type(decoded.body) == "table" then
        local kind = decoded.body.kind
        if kind == "sessions.replay.start" then
          replay_window.set(true)
        elseif kind == "sessions.replay.end" then
          replay_window.set(false)
        end
        local current_replay = replay_window.active()
        local from = (type(decoded.from) == "string") and decoded.from or "engine"
        for _, name in ipairs(plugins_snapshot) do
          if ready_plugins[name] then
            -- Deep-copy the body per peer so a wrapper that mutates
            -- env.body inside its loop doesn't leak into another peer's
            -- view of the same envelope.
            local envs = per_peer_envs[name]
            envs[#envs + 1] = {
              type   = decoded.type,
              from   = from,
              ts     = decoded.ts,
              body   = deep_copy(decoded.body),
              replay = current_replay,
            }
            local tgts = per_peer_targets[name]
            tgts[#tgts + 1] = entry.target
          end
        end
      end
    end
    -- Plugin-origin entries don't appear in the post-refactor flow
    -- (handle_line no longer auto-logs); ignore them defensively.
  end

  -- Fire each peer's batched to_plugin once. Order across peers
  -- mirrors `plugins_snapshot` (registration order), the same order
  -- the per-entry loop used pre-refactor.
  for _, name in ipairs(plugins_snapshot) do
    if ready_plugins[name] then
      call_to_plugin_for_batch(name, per_peer_envs[name], per_peer_targets[name])
    end
  end

  dispatch_hwm = tail_index
end

-- ------------------------------------------------------------------
-- public spawn API
-- ------------------------------------------------------------------

local SPAWN_VALID_KEYS = {
  name        = true,
  command     = true,
  from_plugin = true,
  to_plugin   = true,
}

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

  for k, _ in pairs(cfg) do
    if not SPAWN_VALID_KEYS[k] then
      local hint
      if k == "env" then
        hint = "ncp.spawn: unknown field 'env'; pass values via CLI args inside the command array, e.g. `command = { binary, \"--name\", \"ollama\" }`"
      elseif k == "args" then
        hint = "ncp.spawn: unknown field 'args'; put args inside the command array, e.g. `command = { binary, \"--flag\", \"value\" }`"
      elseif k == "cwd" then
        hint = "ncp.spawn: unknown field 'cwd'; the engine always uses <plugin-dir>/<name>/ as cwd"
      else
        hint = "ncp.spawn: unknown field '" .. tostring(k) .. "'"
      end
      error(hint, 2)
    end
  end

  -- Always register the wrapper (even with nil callbacks) so dispatch
  -- knows the peer is "owned" by a wrapper. A nil callback = framework
  -- default applies.
  plugin_transforms[cfg.name] = {
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
  }

  nefor.plugins.spawn({
    name    = cfg.name,
    command = cfg.command,
  })
end

-- ------------------------------------------------------------------
-- test escape hatches
-- ------------------------------------------------------------------

function M._reset()
  ready_plugins      = {}
  plugin_transforms  = {}
  pending_chat_popups = {}
  dispatch_hwm       = 0
  M._current_log_ref = nil
end

function M._test_set_transforms(name, transforms)
  plugin_transforms[name] = transforms
end

-- Tests sometimes need to mark a peer as ready without going through
-- the JSON handshake. Production code shouldn't need this.
function M._test_set_ready(name, flag)
  if flag then
    ready_plugins[name] = true
  else
    ready_plugins[name] = nil
  end
end

return M
