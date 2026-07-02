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

local M = {}
M.__index = M

-- Reserved emit kinds (namespaced, so they never collide with a factory's
-- fully-qualified declared output tags).
local READY = "mag.ready"
local UNIT = "mag.Unit"
local CAP_INVOKE = "capability.invoke"
local COMPLETE = "mag.complete"
local FAILED = "mag.failed"

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
    log = {
      info = (log.info) or noop,
      warn = (log.warn) or noop,
      error = (log.error) or noop,
    },
    correlation = correlation.new(opts.gen_id),
    instances = {}, -- id -> instance handle (with :deliver)
    machines = {}, -- id -> { port -> firing machine }
    ready = {}, -- id -> true once the factory confirmed ready
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
  else
    self:route_output(id, kind, message)
  end
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
-- routable and drain its pending mailbox through firing, in arrival order.
-- Both mailbox shapes are handled: routed entries ({ __routed, from, kind,
-- message }) and MAG-seed content stored bare by the fold's do_send (a content
-- table carrying its own .kind).
function M:on_ready(id)
  if self.ready[id] then
    return
  end
  self.ready[id] = true
  local queued = self.inventory.take_mailbox(id)
  for _, m in ipairs(queued or {}) do
    if m.__routed then
      self:fire(id, m.from, m.kind, m.message)
    else
      self:fire(id, m.from, m.kind, m)
    end
  end
end

-- Drop all routing state for a killed id (kill drops the mailbox — inventory's
-- do_kill already clears it — plus the firing slots and outstanding capability
-- correlations; actor-model.md, Signals: kill). Wired from the inventory's
-- on_kill seam in init.lua.
function M:forget(id)
  self.machines[id] = nil
  self.ready[id] = nil
  self.instances[id] = nil
  self.correlation:drop_requester(id)
end

return M
