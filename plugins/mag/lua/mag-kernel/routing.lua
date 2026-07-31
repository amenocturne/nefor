-- plugins/mag/lua/mag-kernel/routing.lua — everything between an actor's output and
-- its destination (task: routing, correlation, lazy construction).
--
-- Actors are bus-unaware (actor-model.md): the kernel is their entire world.
-- This module is that world's delivery layer. It composes three primitives —
-- the inventory (lifecycle + routes), the firing machine (firing.lua, input
-- contracts), and correlation (correlation.lua, capability request/response)
-- — into the id-signed message flow:
--
--   * Id-signed delivery. Every outbound message an instance emits carries its
--     actor id; the kernel routes it by looking up the *sender's*
--     routes[output_type] and delivering to each destination id (lowering.md:
--     edges dissolve into per-actor routes; fanout is a multi-element array).
--   * Firing by input contract. A delivered message feeds the destination's
--     firing machine; single fires per message, union on any, product on a
--     complete sender-bound set (per-slot FIFO). Partial product inputs buffer
--     in the machine's slots — a registered spec accepts messages whether or
--     not its instance exists yet. One assembled activation per complete set
--     reaches the instance.
--   * Lazy construction. The instance is built at the actor's FIRST assembled
--     activation (actor-model.md, Lifecycle): activate calls the injected
--     construct hook, the factory confirms with its `mag.ready` emit (surfaced
--     as `mag.actor_ready` — "began work"), and the activation is delivered
--     immediately after. An actor whose contract is never satisfied never
--     constructs.
--   * Kernel-emitted status types. On an activation's successful completion the
--     kernel emits `mag.Unit` along that actor's `mag.Unit` routes; on a
--     suffered failure, the failure-typed output. A factory never returns
--     these and need not know a dependency edge exists (docs/ir.md, Firing).
--   * Capability correlation. A `capability.invoke` emit goes out on the
--     injected bus with a kernel-minted request id; the matching `tool.result`
--     routes back to the requesting actor as a reply activation.
--   * Kill drops the instance, the firing slots, and any correlations. A kill
--     before construction just drops the spec — no instance exists, so no
--     final kill message is delivered.
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
--     { kind = "mag.ready", from }                          — readiness confirm
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
local typed_value = require("typed-value")
local preview = require("preview")

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
-- The human gate's control-plane-bound approval request/cancel
-- (factories/human.lua). Intercepted like RunComplete: neither is a declared
-- output (there is no downstream actor to route to — the consumer is the
-- control plane), so letting them fall through to route_output would drop
-- them silently and no notification would ever surface. Surfaced as run_id-
-- stamped control-plane events (kinds.lua, the approval boundary).
local APPROVAL_REQUEST = kinds.ApprovalRequest
local APPROVAL_CANCEL = kinds.ApprovalCancel

-- Lifecycle event kinds emitted from the delivery layer. actor_ready mirrors
-- observer.lua's canonical set (kept literal to avoid a routing → observer
-- dependency); run_complete is shared through kinds.lua because it drifted
-- against the RunComplete message kind and both must stay in lockstep;
-- run_failed is the unhandled-failure escalation (apply_completion).
local EVT_ACTOR_READY = "mag.actor_ready"
local EVT_RUN_COMPLETE = kinds.run_complete
local EVT_RUN_FAILED = kinds.run_failed
-- Per-actor activity transitions (mirrored in observer.lua's canonical set,
-- kept literal for the same reason as actor_ready). mag.actor_busy fires at
-- activation delivery (after construct, before deliver); mag.actor_idle fires
-- when that activation's completion settles — sync return, async
-- mag.complete / mag.failed, or a capability reply resolving a pending
-- completion — failed settles included, carrying the window's busy_ms.
-- Alternation is strict per actor: an overlapping activation extends the one
-- open window (no nested busy), and a settle with no open window is a no-op.
-- Cost: two control-plane events per activation — acceptable; the session
-- log already carries per-round provider traffic, and activity-honest panel
-- ticking needs exactly these transitions.
local EVT_ACTOR_BUSY = "mag.actor_busy"
local EVT_ACTOR_IDLE = "mag.actor_idle"
local EVT_NODE_PREVIEW = "mag.node_preview"
local EVT_EMISSION_IGNORED = "mag.emission_ignored"
-- The approval boundary's control-plane events (kinds.lua): the intercepted
-- ApprovalRequest / ApprovalCancel emits surface under these kinds, run_id-
-- stamped by the injected events sink like every other control-plane event —
-- the reply (`mag.apply`) needs the run_id to address the right run.
local EVT_APPROVAL_REQUEST = kinds.approval_request
local EVT_APPROVAL_CANCEL = kinds.approval_cancel

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
    -- Authoritative run-context provenance for capability invocations. The
    -- context owns run/session/scope identity; routing contributes actor and
    -- the freshly minted capability id without parsing either opaque value.
    invocation_provenance = opts.invocation_provenance,
    -- Injected lifecycle-event sink (observer.lua's EVENTS set) and per-node
    -- output persistence sink. Both default to no-ops so routing stays pure and
    -- unit-testable; init.lua wires the real emitter and an output-persistence-
    -- backed writer, tests pass capturing stubs.
    events = opts.events or noop,
    persist_output = opts.persist_output or noop,
    observe_output = opts.observe_output or noop,
    settle_result = opts.settle_result or function() return true end,
    -- Injected clock for the busy-window stamps (mag.actor_idle's busy_ms).
    -- init.lua wires the host's nefor.now_ms; pure unit tests pass a fake or
    -- take the zero default.
    now_ms = opts.now_ms or function()
      return 0
    end,
    log = {
      info = (log.info) or noop,
      warn = (log.warn) or noop,
      error = (log.error) or noop,
    },
    correlation = correlation.new(opts.gen_id),
    -- Lazy-construction hook: fn(record) -> instance | nil, err. init.lua
    -- wires registry:construct with the kernel-injected deps; activate calls
    -- it at an actor's first assembled activation (set_construct).
    construct = opts.construct,
    instances = {}, -- id -> instance handle (with :deliver)
    machines = {}, -- id -> { port -> firing machine }
    ready = {}, -- id -> true once the factory confirmed ready
    busy = {}, -- id -> busy-since stamp while an activation window is open
    control_metadata = {}, -- arrival id -> control-plane-only result metadata
    signaling = {}, -- id -> true while a signal handler (kill/drain) is running
    generations = {}, -- id -> emitter generation currently authorized to speak
    result_boundary = nil, -- compiled StoredPort; structural, never a factory
    arrival_seq = 0,
  }, M)
  return self
end

function M:set_result_boundary(boundary)
  self.result_boundary = boundary
end

-- Register the constructed instance for an id. activate binds through here
-- after the construct hook returns; tests bind stub instances directly.
function M:bind(id, instance)
  self.instances[id] = instance
end

-- Register (or replace) the lazy-construction hook after wiring, breaking the
-- construction-order cycle (the hook needs the router's emitter; the router
-- needs the hook).
function M:set_construct(fn)
  self.construct = fn
end

-- The outbound sink for one actor id — its entire world for emitting
-- (actor-model.md). Hand this to a factory's construct as `emit`.
function M:emitter(id)
  local generation = (self.generations[id] or 0) + 1
  self.generations[id] = generation
  return function(message)
    self:on_emit(id, message, generation)
  end
end

function M:preview_emitter(id)
  return function(operation, binding, value)
    local actor = self.inventory.get(id)
    local declaration = actor and self.registry:declaration(actor.factory)
    local validated = declaration and { bindings = declaration.preview_bindings }
    local ok, err = preview.validate_update(validated, operation, binding, value)
    if not ok then
      self.log.warn(string.format(
        "actor '%s' preview update rejected for binding '%s': %s",
        tostring(id), tostring(binding), tostring(err)))
      return false
    end
    self.events({ kind = EVT_NODE_PREVIEW, id = id, operation = operation,
      binding = binding, value = preview.deep_copy(value) })
    return true
  end
end

function M:observe_endpoint(id, endpoint, binding, value)
  self.events({
    kind = EVT_NODE_PREVIEW,
    id = id,
    operation = "set",
    binding_kind = endpoint,
    binding = binding,
    value = preview.deep_copy(value),
  })
  if binding ~= "last" then
    self.events({
      kind = EVT_NODE_PREVIEW,
      id = id,
      operation = "set",
      binding_kind = endpoint,
      binding = "last",
      value = preview.deep_copy(value),
    })
  end
end

-- Dispatch one emitted, id-signed message. Reserved kinds are intercepted;
-- any other kind is a declared output routed by the sender's routes.
function M:on_emit(id, message, generation)
  message = message or {}
  local kind = message.kind
  local actor = self.inventory.get(id)
  local current_generation = self.generations[id]
  local dead = not actor or actor.state ~= "alive"
  local signal_bus_envelope = self.signaling[id] and kind ~= READY
    and kind ~= CAP_INVOKE and kind ~= COMPLETE and kind ~= FAILED
    and kind ~= RUN_COMPLETE and kind ~= APPROVAL_REQUEST
    and kind ~= APPROVAL_CANCEL
  if generation ~= nil and (generation ~= current_generation
      or (dead and not signal_bus_envelope)) then
    self.events({
      kind = EVT_EMISSION_IGNORED,
      from = id,
      message_kind = kind,
      reason = generation ~= current_generation and "stale_generation" or "actor_dead",
      generation = generation,
      current_generation = current_generation,
    })
    return false
  end
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
  elseif kind == APPROVAL_REQUEST then
    -- The human gate raised an approval request (factories/human.lua). Its
    -- consumer is the control plane, not a routed actor: surface it as the
    -- run_id-stamped `mag.approval_request` event. Not persisted as a node
    -- output — the gate's output is the eventual Approved/Rejected exit.
    self.events({
      kind = EVT_APPROVAL_REQUEST,
      from = id,
      correlation = message.correlation,
      prompt = message.prompt,
      subject = message.subject,
    })
  elseif kind == APPROVAL_CANCEL then
    -- The gate retracted an outstanding request (drain). Intercepted before
    -- the signaling branch so it reaches the control plane as a run_id-stamped
    -- event (the raw bus path would lose the run attribution).
    self.events({
      kind = EVT_APPROVAL_CANCEL,
      from = id,
      correlation = message.correlation,
    })
  elseif self.signaling[id] then
    -- A non-reserved emit from an actor whose signal handler (kill/drain) is
    -- running: these are final abort/cancel envelopes bound for a capability
    -- plugin (llm's `<provider>.chat.cancel`), not declared outputs. They must
    -- bypass route_output's declared-tag routing
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
    local arrival, arrival_error = self:factory_arrival(id, kind, message)
    if not arrival then
      self.log.error(arrival_error)
      self.events({
        kind = EVT_RUN_FAILED,
        from = id,
        failure = "typed-output",
        error = arrival_error,
      })
      return
    end
    local observed = {}
    for key, value in pairs(arrival.payload) do observed[key] = value end
    local metadata = self.control_metadata[arrival.arrival_id]
    if metadata and metadata.transcript_delta ~= nil then
      observed.transcript_delta = metadata.transcript_delta
    end
    observed.semantic_type = arrival.type
    observed.semantic_type_id = arrival.type_id
    observed.constructor_id = arrival.constructor_id
    observed.arrival_id = arrival.arrival_id
    local boundary = self.result_boundary
    if boundary and boundary.actor == id and boundary.wire == kind then
      local host = nefor and nefor.semantic_type
      if type(boundary.type_id) == "string" and type(boundary.type) == "table" and
          (type(host) ~= "table" or type(host.accepts) ~= "function"
            or not host.accepts(boundary.type, arrival.type)) then
        self.events({
          kind = EVT_RUN_FAILED,
          from = id,
          failure = "typed-result",
          error = string.format(
            "result boundary '%s.%s' does not accept emitted semantic type '%s'",
            tostring(id), tostring(kind), tostring(arrival.type_id)),
        })
        return
      end
      if not self.settle_result(id, observed) then return false end
    else
      self.persist_output(id, observed)
    end
    self:observe_endpoint(id, "output", kind, observed)
    if self.observe_output(id, kind, observed) == false then return false end
    self:route_output(id, kind, observed, arrival)
  end
end

function M:next_arrival_id()
  self.arrival_seq = self.arrival_seq + 1
  return "arrival:" .. tostring(self.arrival_seq)
end

function M:factory_arrival(id, wire, payload)
  local actor = self.inventory.get(id)
  local endpoint = nil
  for _, output in ipairs((actor and actor.outputs) or {}) do
    if output.wire == wire then
      if endpoint then
        return nil, string.format(
          "actor '%s' emitted ambiguous output wire '%s'", tostring(id), tostring(wire))
      end
      endpoint = output
    end
  end
  if not endpoint and actor and #(actor.outputs or {}) == 0 then
    endpoint = {
      type_id = tostring(wire),
      type = { kind = "named", name = "legacy." .. tostring(wire), arguments = {} },
    }
  end
  if endpoint and (type(endpoint.type_id) ~= "string" or type(endpoint.type) ~= "table") then
    endpoint = {
      type_id = tostring(wire),
      type = { kind = "named", name = "legacy." .. tostring(wire), arguments = {} },
    }
  end
  if not endpoint or type(endpoint.type_id) ~= "string" or type(endpoint.type) ~= "table" then
    return nil, string.format(
      "actor '%s' emitted undeclared typed output wire '%s'", tostring(id), tostring(wire))
  end
  local actual_type = endpoint.type
  local actual_type_id = endpoint.type_id
  local constructor_id = endpoint.type_id
  if endpoint.type.kind == "union" then
    local selected = payload.semantic_type_id or
      (type(payload.value) == "table" and payload.value.type or nil)
    local host = nefor and nefor.semantic_type
    if type(selected) ~= "string" or type(host) ~= "table" or type(host.id) ~= "function" then
      return nil, string.format(
        "actor '%s' emitted a sum without a trusted constructor identity", tostring(id))
    end
    actual_type = nil
    for _, arm in ipairs(endpoint.type.items or {}) do
      if host.id(arm) == selected then actual_type = arm break end
    end
    if not actual_type then
      return nil, string.format(
        "actor '%s' emitted constructor '%s' outside its declared sum",
        tostring(id), tostring(selected))
    end
    actual_type_id = selected
    constructor_id = selected
  end
  if actor.semantic_strict then
    local host = nefor and nefor.semantic_type
    if payload.value == nil or type(host) ~= "table"
        or type(host.validate_value) ~= "function" then
      return nil, string.format(
        "actor '%s' emitted no canonical semantic value on '%s'",
        tostring(id), tostring(wire))
    end
    local semantic_value = payload.semantic_value
    if semantic_value == nil then semantic_value = payload.value end
    local validation = host.validate_value(actual_type, semantic_value)
    if not validation.ok then
      local violation = (validation.violations or {})[1]
      local detail = violation and
        (tostring(violation.path) .. ": " .. tostring(violation.message))
        or ((validation.error or {}).message or "semantic value does not conform")
      return nil, string.format(
        "actor '%s' emitted malformed semantic value on '%s': %s",
        tostring(id), tostring(wire), tostring(detail))
    end
  end
  local routed_payload = payload
  if actor.semantic_strict then
    routed_payload = {
      kind = wire,
      value = payload.value,
    }
    if payload.semantic_value ~= nil then
      routed_payload.semantic_value = payload.semantic_value
    end
  end
  local arrival_id = self:next_arrival_id()
  local inherited_metadata = payload.arrival_id and self.control_metadata[payload.arrival_id]
  if payload.transcript_delta ~= nil then
    self.control_metadata[arrival_id] = { transcript_delta = payload.transcript_delta }
  elseif inherited_metadata ~= nil then
    self.control_metadata[arrival_id] = inherited_metadata
  end
  local control_metadata = self.control_metadata[arrival_id]
  return typed_value.factory({
    arrival_id = arrival_id,
    from = id,
    edge_id = "factory:" .. tostring(id) .. ":" .. tostring(wire),
    type_id = actual_type_id,
    type = actual_type,
    declared_type_id = endpoint.type_id,
    declared_type = endpoint.type,
    constructor_id = constructor_id,
    protocol_wire = wire,
    product_position = -1,
    payload = routed_payload,
    control_metadata = control_metadata,
  })
end

-- The sink's terminal completion signal (factories/sink.lua). It carries the
-- final result and whether the sink's own writer persisted it; surface it to the
-- control plane as a `mag.run_complete` lifecycle event. The sink declares no
-- outputs, so there is nothing to route.
function M:on_run_complete(id, message)
  self.settle_result(id, message.result, message.persist_result)
end

-- Route one id-signed output along the sender's routes[tag]. A tag with no
-- route entry simply goes nowhere; fanout (many dests) needs no special case.
function M:route_output(sender_id, tag, message, source_arrival)
  local sender = self.inventory.get(sender_id)
  if not sender then
    return
  end
  local dests = (sender.routes or {})[tag]
  if not dests then
    return
  end
  for _, destination in ipairs(dests) do
    local source = source_arrival
    if not source then
      local source_type_id = destination.source_type_id or tostring(tag)
      local source_type
      if destination.source_type_id then
        source_type = self:descriptor_for_id(sender_id, source_type_id)
      else
        source_type = {
          kind = "named",
          name = "legacy." .. tostring(tag),
          arguments = {},
        }
      end
      source = typed_value.factory({
        arrival_id = self:next_arrival_id(),
        from = sender_id,
        edge_id = "kernel:" .. tostring(sender_id) .. ":" .. tostring(tag),
        type_id = source_type_id,
        type = source_type,
        constructor_id = source_type_id,
        protocol_wire = tag,
        product_position = -1,
        payload = message,
      })
    end
    local dest = self.inventory.get(destination.actor)
    local input = dest and dest.input
    local accepts = true
    local host = nefor and nefor.semantic_type
    if dest and dest.semantic_strict and input and type(input.type) == "table" and
        type(host) == "table" and type(host.accepts) == "function" then
      accepts = host.accepts(input.type, source.type)
    end
    if accepts then
      self:deliver(destination.actor,
        typed_value.routed(source, destination, input and input.type or source.type))
    end
  end
end

function M:descriptor_for_id(actor_id, type_id)
  local actor = self.inventory.get(actor_id)
  for _, endpoint in ipairs((actor and actor.outputs) or {}) do
    if endpoint.type_id == type_id then return endpoint.type end
  end
  local input = actor and actor.input
  if input and input.type_id == type_id then return input.type end
  error(string.format("actor '%s' has no descriptor for semantic id '%s'",
    tostring(actor_id), tostring(type_id)))
end

-- Deliver one typed message to a destination id. Dead target → dropped as a
-- logged no-op (docs/ir.md: the sender computed the send while the target
-- lived). Live target → fed to the firing machine, constructed or not: the
-- machine buffers partial product inputs, and an assembled activation
-- constructs the instance on demand (activate).
function M:deliver(dest_id, arrival, legacy_tag, legacy_message)
  if not typed_value.is_trusted(arrival) then
    local from = arrival
    local dest = self.inventory.get(dest_id)
    local input = dest and dest.input
    arrival = typed_value.initial({
      arrival_id = self:next_arrival_id(),
      from = from,
      type_id = (input and input.type_id) or tostring(legacy_tag),
      type = (input and input.type) or
        { kind = "named", name = "legacy." .. tostring(legacy_tag), arguments = {} },
      constructor_id = (input and input.type_id) or tostring(legacy_tag),
      protocol_wire = legacy_tag,
      product_position = -1,
      payload = legacy_message,
    })
  end
  local dest = self.inventory.get(dest_id)
  if not dest or dest.state == "dead" then
    self.log.info(string.format("send dropped: target '%s' is dead", tostring(dest_id)))
    return
  end
  self:fire(dest_id, arrival)
end

function M:deliver_initial(dest_id, from, message)
  local dest = self.inventory.get(dest_id)
  if not dest or type(dest.input) ~= "table" then
    local content = message.content or {}
    return self:deliver(dest_id, from, content.kind, content)
  end
  local type_id = message.semantic_type_id or dest.input.type_id or
    ((message.content or {}).kind)
  local descriptor = message.semantic_type or dest.input.type or
    { kind = "named", name = "legacy." .. tostring(type_id), arguments = {} }
  local host = nefor and nefor.semantic_type
  if dest.input.type_id and (type(host) ~= "table"
      or type(host.accepts) ~= "function"
      or not host.accepts(dest.input.type, descriptor)) then
    local detail = string.format(
      "initial message for '%s' has incompatible semantic type id '%s', expected '%s'",
      tostring(dest_id), tostring(type_id), tostring(dest.input.type_id))
    self.events({ kind = EVT_RUN_FAILED, from = dest_id, failure = "typed-input", error = detail })
    return
  end
  local content = message.content
  if type(dest.input.type) == "table" and dest.input.type.kind == "union"
      and type(content) == "table" and type(content.value) == "table"
      and content.value.type == type_id and content.value.value ~= nil then
    local normalized = {}
    for key, value in pairs(content) do normalized[key] = value end
    normalized.value = content.value.value
    content = normalized
  end
  self:deliver(dest_id, typed_value.initial({
    arrival_id = self:next_arrival_id(),
    from = from,
    type_id = type_id,
    type = descriptor,
    declared_type_id = dest.input.type_id,
    declared_type = dest.input.type,
    constructor_id = type_id,
    protocol_wire = (content and content.kind) or dest.input.wire,
    product_position = -1,
    payload = content,
  }))
end

-- Tags delivered by tag, past the declared ports (actor-model.md, The
-- approval boundary): the control plane injects an approval reply at the gate
-- it addresses — replies originate at the chat surface, not upstream actors,
-- so no factory declares an input port for them and no sender edge exists for
-- a firing slot to bind to. A bypass tag reaches a CONSTRUCTED instance
-- directly (a gate with an outstanding request necessarily constructed) and
-- never triggers construction — constructing on a reply would falsely signal
-- "began work" for an actor that was never activated. The unconstructed case
-- is rejected upstream at apply (inventory.lua, validate: a reply can only
-- answer an outstanding request); the warn-drop below is the defensive
-- backstop for direct router use.
local PORT_BYPASS_TAGS = {
  [kinds.ApprovalReply] = true,
}

-- Feed one arrival to the destination's firing machine (the port whose input
-- contract accepts the tag) and dispatch every activation it assembles.
--
-- No port accepting the tag is a WIRING BUG, not a race: apply-time
-- validation catches every statically-visible mismatch (inventory.lua,
-- validate_routes), so anything reaching this branch is a dynamic mismatch
-- (a modification message with an unaccepted kind, a contract drifting
-- mid-run). It used to warn-drop — which starved the destination forever and
-- hung the run with zero pending work (the shipped exhaust bug) — so it now
-- escalates to the control plane as mag.run_failed and the host fails the
-- run. Sends to DEAD targets stay drop-and-log (deliver above — settled race
-- semantics, untouched).
function M:fire(dest_id, arrival, legacy_tag, legacy_message)
  if not typed_value.is_trusted(arrival) then
    local from = arrival
    local dest = self.inventory.get(dest_id)
    local input = dest and dest.input
    arrival = typed_value.initial({
      arrival_id = self:next_arrival_id(),
      from = from,
      type_id = (input and input.type_id) or tostring(legacy_tag),
      type = (input and input.type) or
        { kind = "named", name = "legacy." .. tostring(legacy_tag), arguments = {} },
      constructor_id = (input and input.type_id) or tostring(legacy_tag),
      protocol_wire = legacy_tag,
      product_position = -1,
      payload = legacy_message,
    })
  end
  local ports = self:machines_for(dest_id)
  for port, machine in pairs(ports) do
    if machine:accepts(arrival.protocol_wire) then
      local offered = machine.semantic and arrival or arrival.payload
      for _, activation in ipairs(machine:offer(
        arrival.from, arrival.protocol_wire, offered)) do
        local values = {}
        for index, message in ipairs(activation.messages or {}) do
          values[index] = message.message
        end
        activation.observed_port = port
        activation.observed_value = #values == 1 and values[1] or values
        self:activate(dest_id, activation)
      end
      return
    end
  end
  if PORT_BYPASS_TAGS[arrival.protocol_wire] then
    local instance = self.instances[dest_id]
    if instance and type(instance.deliver) == "function" then
      local completion = instance.deliver({
        shape = "single",
        messages = { {
          from = arrival.from,
          tag = arrival.protocol_wire,
          message = arrival.payload,
          arrival = arrival,
        } },
      })
      self:apply_completion(dest_id, completion)
    else
      self.log.warn(string.format(
        "'%s' dropped: actor '%s' is not constructed (nothing awaits a reply)",
        tostring(arrival.protocol_wire), tostring(dest_id)))
    end
    return
  end
  local detail = string.format(
    "actor '%s' has no input port accepting tag '%s' (sent from '%s'); failing the run",
    tostring(dest_id), tostring(arrival.protocol_wire), tostring(arrival.from))
  self.log.error(detail)
  self.events({
    kind = EVT_RUN_FAILED,
    from = dest_id,
    failure = "no-input-port",
    error = detail,
  })
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
      local semantic = actor.input and actor.input.type_id and
        {
          input_type_id = actor.input.type_id,
          kind = actor.input.type and actor.input.type.kind == "product" and "product"
            or (actor.input.type and actor.input.type.kind == "union" and "union" or "single"),
        } or nil
      if semantic and actor.input.type and actor.input.type.kind == "product" then
        slots = self:derive_slots(id)
      elseif shape.classify(in_shape) == "product" then
        slots = self:derive_legacy_slots(id, in_shape)
      end
      ports[port] = firing.build(in_shape, slots, semantic)
    end
  end
  self.machines[id] = ports
  return ports
end

function M:derive_legacy_slots(dest_id, product_shape)
  local components = {}
  for _, tag in ipairs(shape.tags(product_shape)) do components[tag] = true end
  local edges = {}
  for sender_id, actor in self.inventory.pairs() do
    for _, destinations in pairs(actor.routes or {}) do
      for _, destination in ipairs(destinations) do
        if destination.actor == dest_id and components[destination.wire] then
          edges[#edges + 1] = { sender = sender_id, type = destination.wire }
        end
      end
    end
  end
  return edges
end

-- Derive a product input's slots from the routes topology. Slot identity is
-- the incoming edge (sender, type), not the bare component type: scan every
-- actor's routes for entries that (a) target this actor and (b) deliver on a
-- wire that is a component of the product. Each such (sender, destination
-- wire) is one slot. Source and destination wires may differ because routing
-- performs the typed retag at the edge boundary.
-- This is what makes `(Unit + Unit)` from two upstreams unambiguous — two
-- edges, two sender-bound slots — where keying by the bare type could not tell
-- them apart (docs/ir.md, Firing). Because ids are signed and routes are
-- directional, the binding is a static fact of the topology.
function M:derive_slots(dest_id)
  local edges = {}
  local whole_edges = 0
  local destination_actor = self.inventory.get(dest_id)
  local input_type_id = destination_actor and destination_actor.input
    and destination_actor.input.type_id
  for sender_id, actor in self.inventory.pairs() do
    for _, dests in pairs(actor.routes or {}) do
      for _, destination in ipairs(dests) do
        if destination.actor == dest_id then
          if type(destination.product_position) == "number" and
              destination.product_position >= 0 then
            edges[#edges + 1] = {
              sender = sender_id,
              type = destination.wire,
              edge_id = destination.edge_id,
              product_position = destination.product_position,
            }
          elseif destination.product_position == -1 and
              destination.destination_type_id == input_type_id then
            whole_edges = whole_edges + 1
          end
        end
      end
    end
  end
  -- Application validation guarantees exact product coverage before the
  -- inventory changes. Reaching this backstop means an internal topology
  -- invariant was broken; fail loudly rather than parking a partial product.
  local component_count = destination_actor and destination_actor.input and
    destination_actor.input.type and #(destination_actor.input.type.items or {}) or 0
  if (#edges == 0 and whole_edges == 0) or
      (#edges > 0 and #edges ~= component_count) then
    error(string.format(
      "actor '%s': product input derives %d component slot(s) and %d whole edge(s), but the shape has %d component(s)",
      tostring(dest_id), #edges, whole_edges, component_count))
  end
  return edges
end

-- Hand one assembled activation to the instance — constructing it first when
-- this is the actor's first activation — and apply its completion. The
-- instance emits its declared outputs synchronously through its emitter (so
-- they are already routed by the time deliver returns); the kernel then emits
-- the reserved status type for the completion (docs/ir.md, Firing).
function M:activate(id, activation)
  local instance = self.instances[id] or self:construct_instance(id)
  if not instance or type(instance.deliver) ~= "function" then
    self.log.warn(string.format("actor '%s' has no deliver entry point", tostring(id)))
    return
  end
  self:mark_busy(id)
  if activation.observed_port then
    self:observe_endpoint(id, "input", activation.observed_port, activation.observed_value)
  end
  local completion = instance.deliver(activation)
  self:apply_completion(id, completion)
end

-- Open the actor's busy window at activation delivery (mag.actor_busy — see
-- the activity-event comment at the top). Already busy means an overlapping
-- activation: the one open window extends, no nested busy — the busy/idle
-- alternation stays strict per actor.
function M:mark_busy(id)
  if self.busy[id] then
    return
  end
  self.busy[id] = self.now_ms()
  self.events({ kind = EVT_ACTOR_BUSY, id = id })
end

-- Settle the actor's busy window (mag.actor_idle, carrying the window's
-- busy_ms). A settle with no open window — e.g. a port-bypass delivery that
-- never went through activate — is a no-op, keeping the alternation strict.
function M:mark_idle(id)
  local since = self.busy[id]
  if since == nil then
    return
  end
  self.busy[id] = nil
  self.events({ kind = EVT_ACTOR_IDLE, id = id, busy_ms = self.now_ms() - since })
end

-- Construct the instance for a registered spec via the injected hook — the
-- lazy-construction point, reached exactly at the actor's first satisfied
-- input contract. The factory confirms with its `mag.ready` emit DURING the
-- hook (on_emit → on_ready → the `mag.actor_ready` event), so ready precedes
-- the first delivery. A construct failure must not strand the run — with no
-- readiness deadline, nothing else would ever surface it — so it escalates to
-- the control plane as `mag.run_failed` and the host fails the run.
function M:construct_instance(id)
  if not self.construct then
    return nil
  end
  local record = self.inventory.get(id)
  if not record or record.state ~= "alive" then
    return nil
  end
  local instance, err = self.construct(record)
  if err or not instance then
    local detail = string.format("construct failed for '%s' (factory '%s'): %s",
      tostring(id), tostring(record.factory), tostring(err))
    self.log.error(detail)
    self.events({ kind = EVT_RUN_FAILED, from = id, failure = "construct", error = detail })
    return nil
  end
  self:bind(id, instance)
  return instance
end

-- Emit the kernel-synthesized status type for a completion. Successful
-- completion emits mag.Unit along the actor's mag.Unit routes (dependency
-- edges); mag.Unit with no matching route entry is a silent no-op — an actor
-- with no dependents simply notifies no one. A suffered failure emits the
-- failure-typed output where the sender routes that tag (composed failure
-- handling); an UNROUTED failure must not vanish the same way — nothing else
-- would ever fire, the sink never completes, and the host waits forever on a
-- run that silently died. Escalate it to the control plane as a run failure
-- (mag.run_failed) carrying the failure detail. `nil`/"pending" defers (async
-- capability path).
function M:apply_completion(id, completion)
  if completion == nil then
    return
  end
  if completion == "ok" then
    completion = { status = "ok" }
  end
  -- Any real settle — ok or failed, routed or escalated — closes the busy
  -- window BEFORE the status type routes downstream, so the wire reads
  -- idle(this actor) → busy(dependent) in delivery order.
  if completion.status == "ok" then
    self:mark_idle(id)
    self:route_output(id, UNIT, { kind = UNIT, from = id })
  elseif completion.status == "failed" and completion.failure then
    self:mark_idle(id)
    local sender = self.inventory.get(id)
    local routed = sender and (sender.routes or {})[completion.failure]
    if routed then
      self:route_output(id, completion.failure, { kind = completion.failure, from = id, value = completion.value })
      return
    end
    local value = completion.value
    local detail
    if type(value) == "table" and type(value.error) == "string" and #value.error > 0 then
      detail = value.error
    else
      detail = string.format("actor '%s' failed with %s", tostring(id), tostring(completion.failure))
    end
    self.events({
      kind = EVT_RUN_FAILED,
      from = id,
      failure = completion.failure,
      error = detail,
      value = value,
    })
  end
end

-- An actor's request to a capability plugin. Mint a tracked request id, record
-- the requester (+ its opaque ref), and put a tool.invoke-shaped envelope on
-- the injected bus — request/response correlation as one kernel concern
-- (correlation.lua). `from` stamps the
-- emitting actor's plain address onto the outbound envelope (observability:
-- consumers see WHICH actor is invoking without parsing the scoped
-- correlation id; the run is already identifiable via that id / the run
-- context).
function M:on_capability_invoke(id, message)
  local request_id = self.correlation:open(id, message.ref)
  local invocation = nil
  if type(self.invocation_provenance) == "function" then
    invocation = self.invocation_provenance(id, request_id)
  end
  self.bus_emit({
    kind = "tool.invoke",
    id = request_id,
    from = id,
    name = message.capability,
    args = message.request,
    invocation = invocation,
  })
end

-- The host calls this when a correlated bus response (tool.result-shaped:
-- { id, result | error }) arrives. The reply routes back to the requesting
-- actor as a reply activation. Returns true when the correlation was OURS
-- (open in this router) — even when the reply then drops on a dead requester
-- — so a multi-run host can dispatch a response across run contexts and stop
-- at the owner. An unknown id is not ours: returns false, ignored.
function M:bus_observation(response)
  response = response or {}
  local entry = self.correlation:peek(response.id)
  if not entry then return false end
  local observe = self:preview_emitter(entry.requester)
  observe(response.operation, response.binding, response.value)
  return true
end

function M:bus_response(response)
  response = response or {}
  local entry = self.correlation:close(response.id)
  if not entry then
    return false
  end
  local dest = self.inventory.get(entry.requester)
  if not dest or dest.state == "dead" then
    self.log.info(string.format("capability reply dropped: requester '%s' is dead", tostring(entry.requester)))
    return true
  end
  local instance = self.instances[entry.requester]
  if not instance or type(instance.deliver) ~= "function" then
    return true
  end
  local completion = instance.deliver({
    kind = "reply",
    ref = entry.ref,
    result = response.result,
    error = response.error,
  })
  self:apply_completion(entry.requester, completion)
  return true
end

-- Cancel this run's in-flight work WITHOUT settling any correlation: emit a
-- `tool.cancel { id = request_id }` for every OPEN capability correlation so the
-- real work stops burning — the bridge routes each to the gate (a tool leg →
-- the owning source kills the child / cancels a nested sub-run) or to
-- `<provider>.chat.cancel` (a provider round). No reply is delivered, so no
-- actor re-fires from this call. Returns the number of correlations cancelled.
-- Shared by both interrupt paths: the graceful interrupt adds a settle on top;
-- the terminating interrupt (a dispatched sub-run) cancels only, then the host
-- ends the run failed — the run's llm never gets a reply to re-fire on.
function M:cancel_inflight()
  local ids = self.correlation:pending_ids()
  for _, request_id in ipairs(ids) do
    self.bus_emit({ kind = "tool.cancel", id = request_id })
  end
  return #ids
end

-- GRACEFUL interrupt of this run's in-flight work (NOT a kill). For every OPEN
-- capability correlation in this run:
--   1. cancel the real work (cancel_inflight above): a `tool.cancel` per
--      correlation. Real termination first.
--   2. settle the correlation by delivering a synthesized FAILED reply
--      ("interrupted by user") through the EXISTING reply path (bus_response —
--      close + reply activation). The failure lands on the emitting actor's
--      pending completion in THIS run and routes onward exactly like any tool
--      failure (run-tool → tool-result → llm re-fire; bash node → its failure
--      edge; llm → mag.failed). The run context stays alive and winds down to a
--      real final answer — the no-amnesia path.
-- Ids are snapshotted before settling: a settle may re-fire an actor and open
-- a FRESH correlation (the lead llm's next provider round), which must NOT be
-- interrupted — only the work in flight at interrupt time is. Returns the
-- number of correlations settled. A run with nothing in flight is a clean 0.
function M:interrupt(failure)
  failure = failure or "interrupted by user"
  local ids = self.correlation:pending_ids()
  -- Phase 1: real termination requests for every open correlation.
  for _, request_id in ipairs(ids) do
    self.bus_emit({ kind = "tool.cancel", id = request_id })
  end
  -- Phase 2: synthetic failed settle, reusing the reply-delivery path.
  local settled = 0
  for _, request_id in ipairs(ids) do
    if self:bus_response({ id = request_id, error = failure }) then
      settled = settled + 1
    end
  end
  return settled
end

-- The factory confirmed the id is ready (actor-model.md, Lifecycle). With
-- lazy construction this fires inside construct_instance, at the actor's
-- first activation — ready now MEANS "began work". Surface it as a lifecycle
-- event ahead of the first delivery.
function M:on_ready(id)
  if self.ready[id] then
    return
  end
  self.ready[id] = true
  self.events({ kind = EVT_ACTOR_READY, id = id })
end

-- Read-only readiness probe: has this id constructed and confirmed? True only
-- for actors that began work (fired at least once).
function M:is_ready(id)
  return self.ready[id] == true
end

-- Read-only construction probe: does a bound instance exist for this id?
-- The fold's apply-time validation consults this for control-plane reply
-- injection (inventory.lua: a `mag.ApprovalReply` message can only answer an
-- outstanding request, and an outstanding request implies a constructed gate).
function M:is_constructed(id)
  return self.instances[id] ~= nil
end

-- Hand a dying instance its one final kill message (actor-model.md, Signals:
-- kill). Removal is unconditional; the handler is a courtesy so an actor holding
-- live external work (an open provider request) can abort it. A kill before
-- construction finds no instance and returns false — the spec just drops, no
-- courtesy delivery (nothing exists to receive it). Runs the handler with
-- `signaling[id]` set, so any non-reserved envelope it emits takes the
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
-- set so a bus-bound cancel envelope it emits (an llm's `<provider>.chat.cancel`)
-- reaches the bus by the same raw path; intercepted kinds (mag.complete, the
-- human gate's `mag.ApprovalCancel`) still take their reserved paths — the
-- interception branches in on_emit run before the signaling check. Returns true
-- when a handler ran.
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

-- Queue a user message at an LLM actor's transcript boundary. The factory
-- owns when it is safe to append it: after the current provider exchange and
-- any tool results, immediately before the next provider request.
function M:steer(id, message)
  local instance = self.instances[id]
  if not instance or type(instance.handle_steer) ~= "function" then
    return false
  end
  return instance.handle_steer(message) == true
end

-- Drop all routing state for a killed id — the instance, the firing slots
-- (buffered partial inputs included), and outstanding capability correlations
-- (actor-model.md, Signals: kill). Wired from the inventory's on_kill seam in
-- init.lua, strictly after dispatch_kill.
function M:forget(id)
  self.machines[id] = nil
  self.ready[id] = nil
  self.busy[id] = nil
  self.instances[id] = nil
  self.signaling[id] = nil
  self.correlation:drop_requester(id)
end

return M
