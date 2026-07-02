-- starter/mag-kernel/routing.lua — everything between an actor's output and
-- its destination (task: routing, correlation, pending mailboxes).
--
-- Actors are bus-unaware (actor-model.md): the kernel is their entire world.
-- This module is that world's delivery layer. It composes three primitives —
-- the inventory (lifecycle + routes + mailbox), the firing machine
-- (firing.lua, input contracts), and correlation (correlation.lua, capability
-- request/response) — into the id-signed message flow:
--
--   * Id-signed delivery. Every outbound message an instance emits carries its
--     actor id; the kernel routes it by looking up the *sender's*
--     routes[output_type] and delivering to each destination id (lowering.md:
--     edges dissolve into per-actor routes; fanout is a multi-element array).
--   * Firing by input contract. A delivered message feeds the destination's
--     firing machine; single fires per message, union on any, product on a
--     complete sender-bound set (per-slot FIFO). One assembled activation per
--     complete set reaches the instance.
--   * Kernel-emitted status types. On an activation's successful completion the
--     kernel emits `mag.Unit` along that actor's `mag.Unit` routes; on a
--     suffered failure, the failure-typed output. A factory never returns
--     these and need not know a dependency edge exists (docs/ir.md, Firing).
--   * Capability correlation. A `capability.invoke` emit goes out on the
--     injected bus with a kernel-minted request id; the matching `tool.result`
--     routes back to the requesting actor as a reply activation.
--   * Pending mailboxes. A delivery to a registered-but-not-ready id queues in
--     its inventory mailbox and drains through firing, in arrival order, on
--     ready. Kill drops the mailbox, the firing slots, and any correlations.
--
-- ── the kernel ⇄ factory-instance contract (flagged; primitive tasks build on
--    this) ──────────────────────────────────────────────────────────────────
--
-- Kernel → actor: the instance exposes `deliver(activation) -> completion`.
--   activation is one of
--     graph  : { shape = "single"|"union"|"product",
--                messages = { { from, tag, message }, ... } }
--     reply  : { kind = "reply", ref = <opaque>, result = <r>, error = <e> }
--   completion is
--     "ok" | { status = "ok" }              → kernel emits mag.Unit
--     { status = "failed", failure = <tag>, value = <v> } → kernel emits <tag>
--     nil | { status = "pending" }          → deferred (async capability path);
--                                              completion arrives later as a
--                                              mag.complete / mag.failed emit
--
-- Actor → kernel: the instance emits id-signed messages through the emitter
--   this module hands the factory (M:emitter). Reserved kinds are intercepted;
--   everything else is a declared output routed by type:
--     { kind = "mag.ready", from }                          — ready barrier
--     { kind = "capability.invoke", from, capability, request, ref } — request
--     { kind = "mag.complete", from }                       — async success
--     { kind = "mag.failed", from, failure, value }         — async failure
--     { kind = "<declared output tag>", from, ... }         — routed output
--
-- The host bus surface does not exist yet: `bus_emit` is dependency-injected
-- (init.lua passes a documented stub; tests pass a capturing sink), so these
-- modules stay pure and unit-testable in a bare Lua VM.

local shape = require("shape")
local firing = require("firing")
local correlation = require("correlation")
local kinds = require("kinds")

local M = {}
M.__index = M

-- Reserved emit kinds (namespaced, so they never collide with a factory's
-- fully-qualified declared output tags). The canonical spellings live in
-- kinds.lua; only capability.invoke stays local (a bus-bound request kind, not
-- part of the reserved completion vocabulary).
local READY = kinds.ready
local UNIT = kinds.Unit
local CAP_INVOKE = "capability.invoke"
local COMPLETE = kinds.complete
local FAILED = kinds.failed
-- The sink's terminal run-complete signal (factories/sink.lua). Intercepted
-- here so it reaches the control plane as a lifecycle event rather than routing
-- nowhere (the sink declares no outputs).
local RUN_COMPLETE = kinds.RunComplete

-- Lifecycle event kinds emitted from the delivery layer. actor_ready mirrors
-- observer.lua's canonical set (kept literal to avoid a routing → observer
-- dependency); run_complete is shared through kinds.lua because it drifted
-- against the RunComplete message kind and both must stay in lockstep.
local EVT_ACTOR_READY = "mag.actor_ready"
local EVT_RUN_COMPLETE = kinds.run_complete

local function noop() end

function M.new(opts)
  opts = opts or {}
  local log = opts.log or {}
  local self = setmetatable({
    inventory = assert(opts.inventory, "routing needs an inventory"),
    registry = assert(opts.registry, "routing needs a registry"),
    -- Injected host bus seam. Kept pure by injection: init.lua wires a stub
    -- until the host bus lands; tests pass a capturing sink.
    bus_emit = opts.bus_emit or noop,
    -- Injected lifecycle-event sink (observer.lua's EVENTS set) and per-node
    -- output persistence sink. Both default to no-ops so routing stays pure and
    -- unit-testable; init.lua wires the real emitter and an output-persistence-
    -- backed writer, tests pass capturing stubs.
    events = opts.events or noop,
    persist_output = opts.persist_output or noop,
    log = {
      info = (log.info) or noop,
      warn = (log.warn) or noop,
      error = (log.error) or noop,
    },
    correlation = correlation.new(opts.gen_id),
    instances = {}, -- id -> instance handle (with :deliver)
    machines = {}, -- id -> { port -> firing machine }
    ready = {}, -- id -> true once the factory confirmed ready
    held = nil, -- set of held ids while a program-start barrier is active
    signaling = {}, -- id -> true while a signal handler (kill/drain) is running
  }, M)
  return self
end

-- Register the constructed instance for an id. The factory-construction layer
-- (a sibling task) calls this after registry:construct(name, id, params,
-- router:emitter(id)); tests bind stub instances directly.
function M:bind(id, instance)
  self.instances[id] = instance
end

-- The outbound sink for one actor id — its entire world for emitting
-- (actor-model.md). Hand this to a factory's construct as `emit`.
function M:emitter(id)
  return function(message)
    self:on_emit(id, message)
  end
end

-- Dispatch one emitted, id-signed message. Reserved kinds are intercepted;
-- any other kind is a declared output routed by the sender's routes.
function M:on_emit(id, message)
  message = message or {}
  local kind = message.kind
  if kind == READY then
    self:on_ready(id)
  elseif kind == CAP_INVOKE then
    self:on_capability_invoke(id, message)
  elseif kind == COMPLETE then
    self:apply_completion(id, { status = "ok" })
  elseif kind == FAILED then
    self:apply_completion(id, { status = "failed", failure = message.failure, value = message.value })
  elseif kind == RUN_COMPLETE then
    self:on_run_complete(id, message)
  elseif self.signaling[id] then
    -- A non-reserved emit from an actor whose signal handler (kill/drain) is
    -- running: these are final abort/cancel envelopes bound for a capability
    -- plugin (llm's `<provider>.chat.cancel`, human's `mag.ApprovalCancel`),
    -- not declared outputs. They must bypass route_output's declared-tag routing
    -- — which would drop them (a killed actor is already unrouted; a cancel is
    -- not on any route) — and land straight on the bus. This is the raw-emit
    -- seam; on kill it runs strictly before forget (dispatch_kill), so the
    -- envelope reaches bus_emit while the instance is still bound.
    self.bus_emit(message)
  else
    -- A declared output: persist it to the sender's per-node file (the control
    -- plane reads outputs by path) before routing it downstream. Kernel-
    -- synthesized status types (mag.Unit / failures) go out via apply_completion,
    -- not here, so only real actor outputs are persisted.
    self.persist_output(id, message)
    self:route_output(id, kind, message)
  end
end

-- The sink's terminal completion signal (factories/sink.lua). It carries the
-- final result and whether the sink's own writer persisted it; surface it to the
-- control plane as a `mag.run_complete` lifecycle event. The sink declares no
-- outputs, so there is nothing to route.
function M:on_run_complete(id, message)
  self.events({
    kind = EVT_RUN_COMPLETE,
    from = id,
    result = message.result,
    persisted = message.persisted,
  })
end

-- Route one id-signed output along the sender's routes[tag]. A tag with no
-- route entry simply goes nowhere; fanout (many dests) needs no special case.
function M:route_output(sender_id, tag, message)
  local sender = self.inventory.get(sender_id)
  if not sender then
    return
  end
  local dests = (sender.routes or {})[tag]
  if not dests then
    return
  end
  for _, dest_id in ipairs(dests) do
    self:deliver(dest_id, sender_id, tag, message)
  end
end

-- Deliver one typed message to a destination id. Dead target → dropped as a
-- logged no-op (docs/ir.md: the sender computed the send while the target
-- lived). Not-yet-ready target → queued in its pending mailbox
-- (actor-model.md, Lifecycle). Ready target → fed to the firing machine.
function M:deliver(dest_id, from, tag, message)
  local dest = self.inventory.get(dest_id)
  if not dest or dest.state == "dead" then
    self.log.info(string.format("send dropped: target '%s' is dead", tostring(dest_id)))
    return
  end
  if not self.ready[dest_id] then
    self.inventory.enqueue(dest_id, { __routed = true, from = from, kind = tag, message = message })
    return
  end
  self:fire(dest_id, from, tag, message)
end

-- Feed one arrival to the destination's firing machine (the port whose input
-- contract accepts the tag) and dispatch every activation it assembles.
function M:fire(dest_id, from, tag, message)
  local ports = self:machines_for(dest_id)
  for _, machine in pairs(ports) do
    if machine:accepts(tag) then
      for _, activation in ipairs(machine:offer(from, tag, message)) do
        self:activate(dest_id, activation)
      end
      return
    end
  end
  self.log.warn(string.format("actor '%s' has no input port accepting tag '%s'", tostring(dest_id), tostring(tag)))
end

-- Lazily build (and cache) the per-port firing machines for an actor from its
-- factory's declared inputs. Product ports get sender-bound slots derived from
-- the current routes topology (derive_slots). Built on first use, by which
-- time upstream actors and their routes are in the inventory.
function M:machines_for(id)
  local existing = self.machines[id]
  if existing then
    return existing
  end
  local actor = self.inventory.get(id)
  local decl = actor and self.registry:declaration(actor.factory)
  local ports = {}
  if decl and type(decl.inputs) == "table" then
    for port, in_shape in pairs(decl.inputs) do
      local slots = nil
      if shape.classify(in_shape) == "product" then
        slots = self:derive_slots(id, in_shape)
      end
      ports[port] = firing.build(in_shape, slots)
    end
  end
  self.machines[id] = ports
  return ports
end

-- Derive a product input's slots from the routes topology. Slot identity is
-- the incoming edge (sender, type), not the bare component type: scan every
-- actor's routes for entries that (a) target this actor and (b) carry a tag
-- that is a component of the product. Each such (sender, tag) is one slot.
-- This is what makes `(Unit + Unit)` from two upstreams unambiguous — two
-- edges, two sender-bound slots — where keying by the bare type could not tell
-- them apart (docs/ir.md, Firing). Because ids are signed and routes are
-- directional, the binding is a static fact of the topology.
function M:derive_slots(dest_id, product_shape)
  local components = {}
  for _, t in ipairs(shape.tags(product_shape)) do
    components[t] = true
  end
  local edges = {}
  for sender_id, actor in self.inventory.pairs() do
    for tag, dests in pairs(actor.routes or {}) do
      if components[tag] then
        for _, d in ipairs(dests) do
          if d == dest_id then
            edges[#edges + 1] = { sender = sender_id, type = tag }
          end
        end
      end
    end
  end
  -- Arity check: a well-lowered product has exactly one sender-bound slot per
  -- component (docs/ir.md, Firing: slot identity is the incoming edge). A
  -- mismatch means the routes topology under-/over-fills the product — a
  -- lowering bug that would otherwise leave the actor silently unable to fire
  -- (too few edges) or assembling ill-defined sets (too many). Surface it.
  local component_count = #shape.tags(product_shape)
  if #edges ~= component_count then
    self.log.warn(string.format(
      "actor '%s': product input derives %d sender-bound slot(s) but the shape has %d component(s)",
      tostring(dest_id), #edges, component_count))
  end
  return edges
end

-- Hand one assembled activation to the instance and apply its completion. The
-- instance emits its declared outputs synchronously through its emitter (so
-- they are already routed by the time deliver returns); the kernel then emits
-- the reserved status type for the completion (docs/ir.md, Firing).
function M:activate(id, activation)
  local instance = self.instances[id]
  if not instance or type(instance.deliver) ~= "function" then
    self.log.warn(string.format("actor '%s' has no deliver entry point", tostring(id)))
    return
  end
  local completion = instance.deliver(activation)
  self:apply_completion(id, completion)
end

-- Emit the kernel-synthesized status type for a completion. Successful
-- completion emits mag.Unit along the actor's mag.Unit routes (dependency
-- edges); a suffered failure emits the failure-typed output. Routing a status
-- with no matching route entry is a silent no-op — an actor with no dependents
-- simply notifies no one. `nil`/"pending" defers (async capability path).
function M:apply_completion(id, completion)
  if completion == nil then
    return
  end
  if completion == "ok" then
    completion = { status = "ok" }
  end
  if completion.status == "ok" then
    self:route_output(id, UNIT, { kind = UNIT, from = id })
  elseif completion.status == "failed" and completion.failure then
    self:route_output(id, completion.failure, { kind = completion.failure, from = id, value = completion.value })
  end
end

-- An actor's request to a capability plugin. Mint a tracked request id, record
-- the requester (+ its opaque ref), and put a tool.invoke-shaped envelope on
-- the injected bus. This owns as one kernel concern what agentic-loop spread
-- across pending_runs / tool_id_to_key / firing_to_node.
function M:on_capability_invoke(id, message)
  local request_id = self.correlation:open(id, message.ref)
  self.bus_emit({
    kind = "tool.invoke",
    id = request_id,
    name = message.capability,
    args = message.request,
  })
end

-- The host calls this when a correlated bus response (tool.result-shaped:
-- { id, result | error }) arrives. The reply routes back to the requesting
-- actor as a reply activation. An unknown id is not ours and is ignored; a
-- dead requester drops the reply as a logged no-op.
function M:bus_response(response)
  response = response or {}
  local entry = self.correlation:close(response.id)
  if not entry then
    return
  end
  local dest = self.inventory.get(entry.requester)
  if not dest or dest.state == "dead" then
    self.log.info(string.format("capability reply dropped: requester '%s' is dead", tostring(entry.requester)))
    return
  end
  local instance = self.instances[entry.requester]
  if not instance or type(instance.deliver) ~= "function" then
    return
  end
  local completion = instance.deliver({
    kind = "reply",
    ref = entry.ref,
    result = response.result,
    error = response.error,
  })
  self:apply_completion(entry.requester, completion)
end

-- The factory confirmed the id is ready (actor-model.md, Lifecycle). Mark it
-- routable, then drain its pending mailbox — unless a program-start barrier is
-- holding, in which case draining is deferred until the barrier releases (so
-- initial messages are delivered strictly after every actor has readied;
-- docs/ir.md, Running a program — the two-step handshake).
function M:on_ready(id)
  if self.ready[id] then
    return
  end
  self.ready[id] = true
  -- Lifecycle: the factory confirmed this id ready (actor-model.md). Surface it
  -- as a lifecycle event before any draining, so observers see the actor become
  -- routable ahead of the deliveries that drain into it (even when a barrier
  -- defers the drain itself).
  self.events({ kind = EVT_ACTOR_READY, id = id })
  if self.held then
    self.held[id] = true
    return
  end
  self:drain_pending(id)
end

-- Drain an id's pending mailbox through firing, in arrival order. Both mailbox
-- shapes are handled: routed entries ({ __routed, from, kind, message }) and
-- MAG-seed content stored bare (a content table carrying its own .kind).
function M:drain_pending(id)
  local queued = self.inventory.take_mailbox(id)
  for _, m in ipairs(queued or {}) do
    if m.__routed then
      self:fire(id, m.from, m.kind, m.message)
    else
      self:fire(id, m.from, m.kind, m)
    end
  end
end

-- Read-only readiness probe — the barrier consults this to decide when the
-- whole constellation has confirmed (barrier.lua). Readiness is owned here in
-- the delivery layer: on_ready is its sole writer and deliver its sole reader,
-- so there is one fire-vs-queue decision point (the fold delegates its live
-- sends here rather than judging readiness itself; see inventory do_send).
function M:is_ready(id)
  return self.ready[id] == true
end

-- Program-start barrier hooks. begin_barrier makes on_ready defer draining;
-- release_barrier drains every held id's mailbox, delivering all initial
-- messages at once, after the barrier has confirmed the constellation ready.
function M:begin_barrier()
  self.held = self.held or {}
end

function M:release_barrier()
  local held = self.held
  self.held = nil
  if held then
    for id in pairs(held) do
      self:drain_pending(id)
    end
  end
end

-- Hand a dying instance its one final kill message (actor-model.md, Signals:
-- kill). Removal is unconditional; the handler is a courtesy so an actor holding
-- live external work (an open provider request) can abort it. Runs the handler
-- with `signaling[id]` set, so any non-reserved envelope it emits takes the
-- raw-emit path to the bus (on_emit) instead of route_output — which would drop
-- it, the id being already unrouted. Called from the inventory's on_kill seam
-- BEFORE forget (init.lua), so the abort envelope reaches bus_emit while the
-- instance is still bound: emit-before-forget.
function M:dispatch_kill(id)
  local instance = self.instances[id]
  if not instance or type(instance.handle_kill) ~= "function" then
    return false
  end
  self.signaling[id] = true
  instance.handle_kill()
  self.signaling[id] = nil
  return true
end

-- Graceful drain (actor-model.md, Signals: drain / SIGTERM). The kernel exposes
-- this as a distinct op (init.lua `drain(id)`) — it is NEVER auto-called from
-- kill. Calls the instance's drain handler where declared, with `signaling[id]`
-- set so a bus-bound cancel envelope it emits (human's `mag.ApprovalCancel`)
-- reaches the bus by the same raw path; reserved acks (mag.complete) still route
-- normally. Returns true when a handler ran.
function M:drain(id)
  local instance = self.instances[id]
  if not instance or type(instance.handle_drain) ~= "function" then
    return false
  end
  self.signaling[id] = true
  instance.handle_drain()
  self.signaling[id] = nil
  return true
end

-- Drop all routing state for a killed id (kill drops the mailbox — inventory's
-- do_kill already clears it — plus the firing slots and outstanding capability
-- correlations; actor-model.md, Signals: kill). Wired from the inventory's
-- on_kill seam in init.lua, strictly after dispatch_kill.
function M:forget(id)
  self.machines[id] = nil
  self.ready[id] = nil
  self.instances[id] = nil
  self.signaling[id] = nil
  self.correlation:drop_requester(id)
end

return M
