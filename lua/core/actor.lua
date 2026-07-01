-- starter/actor.lua — Lua-side actor runtime.
--
-- ## What this is
--
-- A minimal runtime for "Lua-resident plugins" — modules that participate
-- in the broadcast bus exactly like Rust subprocess plugins do. Each
-- registered actor exposes a `receive_msg(envelope)` function that the
-- runtime calls for every wire envelope on the bus. Actors emit by
-- calling their own `send_msg(...)` (purely side-effecting, plugin-
-- internal) which typically calls `nefor.engine.send(...)` to put a wire
-- envelope on the bus.
--
-- The runtime is intentionally tiny:
--
--   * `actor.install()` — subscribe to the bus once, drive dispatch.
--   * `actor.spawn(spec)` — register an actor (a table with `name` +
--     `receive_msg`).
--   * No subscription tables, no addressing, no targeted routing. Every
--     actor sees every envelope and filters in `receive_msg`.
--
-- ## Why plugin-as-data
--
-- Each module that wants to participate as an actor returns a table with
-- the actor shape:
--
--     return {
--       name        = "sessions",
--       receive_msg = function(entry) ... end,
--       send_msg    = function(internal) ... end,
--       -- + whatever helpers / public methods the module exposes
--     }
--
-- `init.lua` then calls `actor.spawn(require("sessions"))` to register
-- it. The runtime never inspects fields beyond `name` and `receive_msg`;
-- everything else is plugin-internal.
--
-- ## Dispatch model — queue + drain
--
-- When an inbound envelope arrives at the bus, the runtime:
--
--   1. Marks itself as `draining`.
--   2. Calls every actor's `receive_msg(envelope)` in registration order.
--   3. While the outbound queue is non-empty, pops the next envelope and
--      repeats step 2.
--   4. Clears the `draining` flag.
--
-- Outbound emissions during a `receive_msg` invocation arrive at the
-- runtime via the same bus subscription (because `nefor.engine.send`
-- broadcasts back to in-VM subscribers on next tick). The `draining`
-- flag prevents nested entry into the dispatch loop — re-entrant arrivals
-- are queued and drained after the current loop completes.
--
-- This guarantees:
--
--   * Emission order is preserved across the queue, so an actor that
--     emits `session_start` then several replay envelopes can rely on
--     other actors seeing `session_start` first.
--   * No actor's `receive_msg` is re-entered while one of its prior
--     calls is still on the stack.
--   * Cross-actor cascades remain *possible* (an actor reacting to X
--     by emitting Y that triggers a third actor that emits X again),
--     but they are observable on the bus log via the `from` stamp.
--
-- ## Lifecycle
--
-- Engine shutdown arrives as a synthesized wire envelope of kind
-- `engine.shutdown`, so actors handle it via `receive_msg` like any
-- other message. No separate teardown hook.
--
-- ## Anti-patterns (will silently degrade behaviour)
--
--   * An actor calling another actor's methods directly (synchronous
--     cross-module coupling). All inter-actor communication goes via
--     the bus.
--   * An actor declaring a subscription pattern. Filter in `receive_msg`.

local M = {
  actors = {},
  outbound_queue = {},
  draining = false,
  installed = false,
}

-- Register an actor. Two shapes:
--
--   * Pure-Lua actor — no `command` field. Lives entirely in this VM;
--     receive_msg sees every wire envelope, send_msg emits via
--     nefor.engine.send.
--
--   * Rust-binary wrapper — `command` is a table the engine spawns as
--     a subprocess (same shape as `nefor.plugins.spawn`'s `command`
--     argument). The binary participates on the bus through the
--     broker's stdin/stdout pipes; the wrapper actor's receive_msg
--     can additionally translate or react to that traffic. A wrapper
--     with a no-op receive_msg is just "spawn this binary."
--
--     Optionally `from_plugin` and `to_plugin` envelope transforms
--     translate between the plugin's native wire shape and the
--     canonical bus shape. These are the same hooks `ncp.spawn`
--     accepts (and use the same machinery): when a wrapper supplies
--     them, `actor.spawn` routes the spawn through `ncp.spawn` so
--     the protocol layer registers the transforms and applies them
--     at ingress / per-target egress. Without transforms we use
--     `nefor.plugins.spawn` directly to skip the ncp transform-table
--     bookkeeping.
function M.spawn(spec)
  if type(spec) ~= "table" then
    error("actor.spawn: spec must be a table", 2)
  end
  if type(spec.name) ~= "string" or spec.name == "" then
    error("actor.spawn: spec.name must be a non-empty string", 2)
  end
  if type(spec.receive_msg) ~= "function" then
    error("actor.spawn: spec.receive_msg must be a function", 2)
  end
  if spec.from_plugin ~= nil and type(spec.from_plugin) ~= "function" then
    error("actor.spawn: spec.from_plugin must be a function or nil", 2)
  end
  if spec.to_plugin ~= nil and type(spec.to_plugin) ~= "function" then
    error("actor.spawn: spec.to_plugin must be a function or nil", 2)
  end

  M.actors[#M.actors + 1] = spec

  if spec.command then
    if spec.from_plugin or spec.to_plugin then
      -- Route through ncp.spawn so plugin_transforms picks up the
      -- ingress/egress hooks. Loaded lazily to avoid a require cycle
      -- when actor.lua is loaded standalone (e.g. tests that don't
      -- exercise transforms).
      local ncp = require("core.ncp")
      ncp.spawn {
        name        = spec.name,
        command     = spec.command,
        from_plugin = spec.from_plugin,
        to_plugin   = spec.to_plugin,
      }
    else
      nefor.plugins.spawn { name = spec.name, command = spec.command }
    end
  end
end

-- Internal: run one envelope through every actor.
local function dispatch_one(envelope)
  for _, a in ipairs(M.actors) do
    local ok, err = pcall(a.receive_msg, envelope)
    if not ok and nefor.log and nefor.log.error then
      nefor.log.error("actor: receive_msg raised", {
        actor = a.name,
        error = tostring(err),
      })
    end
  end
end

-- Internal: enter the dispatch loop with `envelope` as the seed.
-- Drains any envelopes queued during dispatch before returning.
local function enter_dispatch(envelope)
  if M.draining then
    -- Re-entrant arrival; the outer loop will pick it up.
    M.outbound_queue[#M.outbound_queue + 1] = envelope
    return
  end
  M.draining = true
  dispatch_one(envelope)
  while #M.outbound_queue > 0 do
    local next_env = table.remove(M.outbound_queue, 1)
    dispatch_one(next_env)
  end
  M.draining = false
end

-- Install the runtime. Subscribes to the bus once and to engine
-- shutdown lifecycle once. Idempotent.
--
-- Call BEFORE the first `actor.spawn(...)` so the bus subscription is
-- live when actors start running their boot code (which may emit
-- envelopes the bus needs to route).
function M.install()
  if M.installed then return end

  if nefor.bus and nefor.bus.on_event then
    -- Pattern "*" → KindPattern::Prefix("") in the engine binding,
    -- matching every kind. The actor.lua runtime is the SOLE bus
    -- subscriber for actor traffic; per-actor filters live in
    -- receive_msg.
    nefor.bus.on_event("*", function(entry) enter_dispatch(entry) end)
  end

  if nefor.events and nefor.events.on then
    -- Synthesize a wire envelope for engine shutdown so actors handle
    -- it via receive_msg like any other message. Manifesto §3e lists
    -- `shutdown` as a system kind on the wire; engine.shutdown here is
    -- the in-VM analogue for Lua actors that don't have a stdin.
    nefor.events.on("shutdown", function(_payload)
      local now = nefor.engine.now()
      local ok, payload = pcall(nefor.json.encode, {
        type = "shutdown",
        from = "engine",
        ts   = now,
        body = { kind = "engine.shutdown" },
      })
      if not ok then return end
      enter_dispatch({
        ts      = now,
        origin  = "engine",
        payload = payload,
      })
    end)
  end

  M.installed = true
end

-- Build an identity-passthrough actor spec for a Rust-binary plugin
-- that speaks the canonical wire shape directly (no translation).
--
-- Generic shape — shared by every plugin whose lua-side wrapper is
-- pure passthrough (basic-tools, reasoner-graph,
-- and any future binary that needs no domain translation):
--
--   * `from_plugin`: re-emit every envelope onto the bus via
--     `nefor.engine.send`, defaulting `from` to `name` when missing.
--   * `to_plugin`: deliver to the binary via `nefor.engine.deliver`,
--     skipping `env.replay` (stateful binaries must not see resume-
--     time replays) and skipping self-emissions (`env.from == name`,
--     so the binary doesn't see its own bus echoes).
--   * `receive_msg`: no-op (the wrapper has no Lua-side state).
--
-- Plugin-specific compositors that need translation primitives or
-- orchestrator coupling build their own spec inline — this helper is
-- the right answer ONLY when the wrapper is genuinely identity
-- passthrough.
--
-- Args:
--   name    — bus identity for the wrapper actor. Required.
--   command — argv array used to spawn the binary. Required.
function M.identity_spec(name, command)
  if type(name) ~= "string" or name == "" then
    error("actor.identity_spec: name must be a non-empty string", 2)
  end
  if type(command) ~= "table" then
    error("actor.identity_spec: command must be a table", 2)
  end

  local json = nefor.json

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      if type(env.body) == "table" then
        nefor.engine.send(json.encode({
          type = "event",
          from = env.from or name,
          ts   = nefor.engine.now(),
          body = env.body,
        }))
      end
    end
  end

  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if not env.replay and env.from ~= name then
        -- Strip framework-only fields (`replay`, …) when encoding for
        -- the wire — the protocol parser rejects unknown fields.
        nefor.engine.deliver(name, json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        }))
      end
    end
  end

  return {
    name        = name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

-- Test-only: clear all registered actors and reset draining state.
-- Production code should not call this.
function M._reset()
  M.actors = {}
  M.outbound_queue = {}
  M.draining = false
  M.installed = false
end

return M
