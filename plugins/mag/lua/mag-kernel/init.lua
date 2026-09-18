-- plugins/mag/lua/mag-kernel/init.lua — MAG actor-kernel entry.
--
-- Loaded by the `mag` plugin's embedded Lua VM at startup. This module is
-- the wiring layer: it adapts the host `nefor` surface into the plain
-- dependencies the kernel modules expect, builds the factory registry (the
-- trait layer composition validates against — see registry.lua, shape.lua),
-- and manages RUN CONTEXTS: one per `mag.execute`, each with its own
-- inventory (the fold over graph modifications — see inventory.lua,
-- plugins/mag/docs/actor-model.md, docs/ir.md), router, modification log,
-- and observer. Runs are concurrent: a context is created at begin_run,
-- lives while its constellation works, and is dropped at end_run
-- (complete / failed / superseded). Nothing crosses contexts — routes,
-- sends, correlations, and firing state all resolve within one run.
--
-- `nefor.log` is a host binding that writes to the plugin's tracing
-- subscriber (stderr). The kernel must never write to stdout — that is the
-- NCP wire. The host sets `package.path` to this directory before loading,
-- so sibling modules resolve by bare name (`require("inventory")`).

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local modlog = require("modlog")
local observer = require("observer")
local plain_data = require("plain-data")
local operations = require("operations")
local Topology = require("topology")
local stub = require("factories.stub")
local sink = require("factories.sink")
local source = require("factories.source")
local human = require("factories.human")
local llm = require("factories.llm")
local structured_output = require("factories.structured-output")
local dynamic_each = require("factories.dynamic-each")
local dynamic_index = require("factories.dynamic-index")
local dynamic_output = require("factories.dynamic-output")
local dynamic_all = require("factories.dynamic-all")
local retry_gate = require("factories.retry-gate")

-- Shared per-node output persistence (lua/libs/output-persistence). The mag
-- plugin host currently exposes only nefor.log (plugins/mag/src/kernel.rs,
-- install_nefor) and points package.path at the kernel directory alone, so this
-- require degrades to nil there and persistence becomes a no-op. Expose the lib
-- on package.path plus nefor.fs/json/sessions on the host to activate it.
local ok_persist, persistence = pcall(require, "output-persistence")
if not ok_persist then
  nefor.log("[warn] output-persistence lib not on package.path; per-node "
    .. "output persistence disabled (runs will report persisted=false): "
    .. tostring(persistence))
  persistence = nil
end
local run_tool = require("factories.run-tool")
local tool_result = require("factories.tool-result")
local adapter = require("factories.adapter")
local process = require("factories.process")
local worktree_create = require("factories.worktree-create")
local worktree_open = require("factories.worktree-open")

-- Adapt the host's single `nefor.log(msg)` function into the leveled sink
-- the kernel modules expect. Level travels as a prefix (plus the run scope,
-- so interleaved concurrent-run logs stay attributable) — the one host
-- binding covers info/warn/error without a wider native surface.
local function make_logger(scope)
  local function at(level)
    return function(msg)
      nefor.log(string.format("[%s] [%s] %s", level, scope, msg))
    end
  end
  return { info = at("info"), warn = at("warn"), error = at("error") }
end

-- Build the registry and seed the factories shipped with the kernel. The
-- registry is read-only after seeding and shared by every run context.
local function build_registry()
  local reg = Registry.new()
  local function seed(mod)
    local _, err = reg:register({ declaration = mod.declaration, construct = mod.construct })
    if err then
      error("mag-kernel: failed to register factory '" .. tostring(mod.declaration and mod.declaration.name) .. "': " .. err)
    end
  end
  seed(stub)
  seed(sink)
  seed(source)
  seed(human)
  seed(llm)
  seed(structured_output)
  seed(dynamic_each)
  seed(dynamic_index)
  seed(dynamic_output)
  seed(dynamic_all)
  seed(retry_gate)
  seed(run_tool)
  seed(tool_result)
  seed(adapter)
  seed(process.exec)
  seed(process.script)
  seed(worktree_create)
  seed(worktree_open)
  return reg
end

nefor.log("mag-kernel loading")

local registry = build_registry()

-- ── run contexts ────────────────────────────────────────────────────────────
--
-- runs: run_id → context. Each context is a complete kernel-in-miniature:
-- inventory + router + modlog + observer, plus the host-facing run state
-- (terminal captures, last persisted path). A fresh context IS starting from
-- NullGraph — this replaces the old kill-sweep-and-clear a single global
-- constellation needed per execute.
--
-- Wire-id scoping: two concurrent runs of the same program author identical
-- actor ids, so anything the kernel puts on the SHARED bus that must resolve
-- back to one run carries the run's scope token as a `<scope>/` prefix:
--
--   * capability correlation ids:  r3/cap-7
--   * provider chat handles:       r3/agent.llm@r2
--
-- The scope token `r<K>` is kernel-session-monotone (never reused, even
-- across begin_run/end_run of the same run_id). The prefixed strings stay
-- opaque downstream — the bridge and the providers key on exact match, never
-- parse. Inside a context ids stay unscoped: actors, routes, refs, and events
-- all speak the program's own names; scoping happens only at the bus seam.
local runs = {}
local run_seq = 0

-- Modification-log JSONL sink (one line per entry; docs/ir.md). No host fs/json
-- surface yet, so entries are traced until it lands; the in-memory log is
-- always retained regardless.
local function persist_modlog_entry(entry)
  nefor.log(string.format("[modlog] #%d %s",
    tonumber(entry.seq) or -1, tostring(entry.outcome)))
end

local function has_model_context(factory)
  return factory == "nefor.factory.llm" or factory == "llm"
      or factory == "nefor.factory.structured-output" or factory == "structured-output"
      or factory == "nefor.factory.run-tool" or factory == "run-tool"
end

local function lower_reasoning_effort(value)
  if value == nil then return nil end
  if type(value) == "string" then
    if value == "" then return nil, "reasoning_effort must be a non-empty string when present" end
    return value
  end
  if type(value) ~= "table" or type(value.present) ~= "boolean"
      or type(value.value) ~= "string" then
    return nil, "reasoning_effort must be a string or { present = Bool, value = String }"
  end
  local fields = 0
  for key in pairs(value) do
    if key ~= "present" and key ~= "value" then
      return nil, "reasoning_effort record has unknown field " .. tostring(key)
    end
    fields = fields + 1
  end
  if fields ~= 2 then
    return nil, "reasoning_effort record must contain exactly present and value"
  end
  if value.present then
    if value.value == "" then
      return nil, "reasoning_effort present=true requires a non-empty value"
    end
    return value.value
  end
  if value.value ~= "" then
    return nil, "reasoning_effort present=false requires an empty value"
  end
  return nil
end

local function lower_model_profile(value)
  if value == nil then return nil end
  if type(value) ~= "table" or type(value.present) ~= "boolean"
      or type(value.value) ~= "string" then
    return nil, "model_profile must be { present = Bool, value = String }"
  end
  local fields = 0
  for key in pairs(value) do
    if key ~= "present" and key ~= "value" then
      return nil, "model_profile record has unknown field " .. tostring(key)
    end
    fields = fields + 1
  end
  if fields ~= 2 then
    return nil, "model_profile record must contain exactly present and value"
  end
  if value.present then
    if value.value == "" then
      return nil, "model_profile present=true requires a non-empty value"
    end
    return value.value
  end
  if value.value ~= "" then
    return nil, "model_profile present=false requires an empty value"
  end
  return nil
end

-- Build one run context. `meta` carries the host-provided run identity
-- (run_id/run_name/session_id/model_snapshot) — injected, never ambient
-- (docs/ir.md).
local apply_with_logical_nodes
local apply_typed_delta

local function new_run_context(meta)
  run_seq = run_seq + 1
  local scope = "r" .. tostring(run_seq)
  local log = make_logger(scope)

  local ctx = {
    scope = scope,
    run_id = meta.run_id,
    run_name = meta.run_name,
    session_id = meta.session_id,
    principal = meta.principal,
    model_snapshot = type(meta.model_snapshot) == "table"
      and plain_data.copy(meta.model_snapshot) or nil,
    conversation_id = meta.conversation_id
      or (tostring(meta.session_id or "sessionless") .. "/" .. tostring(meta.run_id)),
    actor_conversations = {},
    last_output_path = nil,
    run_complete = nil,
    run_complete_taken = false,
    terminal_settlement = nil,
    run_failed = nil,
    operations = {},
    operation_queue = {},
    operation_draining = false,
    pending_completion = nil,
    emission_seq = 0,
    observation_seq = 0,
    operation_error = nil,
    operation_failed = false,
    logical_paths = {},
    logical_actors = {},
  }

  -- Injected lifecycle-event sink (observer.lua's EVENTS set, plus routing's
  -- ready/run-complete). Every lifecycle event is broadcast on the NCP bus as
  -- a Body::Event (via the host's nefor.emit queue, drained by the plugin
  -- after each kernel call) and stamped with this run's id — every
  -- kernel→control-plane event carries run_id, so consumers key overlapping
  -- runs apart. The run-complete event additionally carries the sink's
  -- persisted output PATH (control plane reads paths, never node data) and is
  -- captured so the host can settle the execute reply.
  local function emit_event(event)
    if type(event) ~= "table" then
      return
    end
    event.run_id = ctx.run_id
    ctx.observation_seq = ctx.observation_seq + 1
    event.observation_seq = ctx.observation_seq
    event.at_ms = type(nefor.now_ms) == "function" and nefor.now_ms() or nil
    if event.kind == observer.EVENTS.run_complete then
      -- Attach the path the sink's writer persisted this run (recorded by
      -- persist_output below) and stash the signal — result included — for
      -- take_run_complete. The sink's own `persisted` flag only says a writer
      -- was wired; the kernel knows whether a write actually landed (the
      -- persistence lib may be absent or the write may have failed), so the
      -- flag surfaced to the control plane is recomputed from that truth.
      event.output_path = ctx.last_output_path
      event.persisted = ctx.last_output_path ~= nil
      -- Terminal acceptance is owned by settle_result below; event
      -- publication cannot replace or repopulate the accepted value.
    elseif event.kind == observer.EVENTS.run_failed then
      -- An unhandled actor failure (routing.lua apply_completion). Stash it
      -- for take_run_failed so the host fails the run with the detail
      -- surfaced.
      ctx.run_failed = {
        error = event.error,
        failure = event.failure,
        from = event.from,
      }
    end
    nefor.emit(event)
  end

  -- Per-node output persistence keyed by actor id, reusing output-persistence's
  -- session layout (sessions/<id>/mag/runs/<run>/<node>.output). The run DIR is
  -- keyed by run_id, not run_name: the lead's run_name is the bare program name
  -- and two concurrent runs of the same program would collide on it, while the
  -- run_id is minted unique per dispatch (and embeds the name, so the layout
  -- stays readable). Consumers are unaffected — the control plane reads output
  -- paths off the wire, never reconstructs them.
  local function persist_output(node_id, output)
    if not persistence then
      return
    end
    local persisted = persistence.persist(
      {
        run_id = ctx.run_id,
        run_name = ctx.run_id,
        session_id = ctx.session_id,
        node_id = node_id,
      },
      output)
    if type(persisted) == "table" and type(persisted.output_path) == "string" then
      ctx.last_output_path = persisted.output_path
      return persisted
    end
    return nil
  end

  -- The sole terminal linearization point. Persistence and completion become
  -- visible together, and the accepted value remains latched after host take.
  local function publish_completion(settlement)
    persist_output(settlement.from, settlement.persisted_result or settlement.result)
    local completion = {
      output_path = ctx.last_output_path,
      persisted = ctx.last_output_path ~= nil,
      result = settlement.result,
    }
    settlement.completion = completion
    ctx.terminal_settlement = settlement
    ctx.run_complete = completion
    emit_event({
      kind = observer.EVENTS.run_complete,
      from = settlement.from,
      result = completion.result,
      persisted = completion.persisted,
    })
  end

  local function settle_result(node_id, result, persisted_result)
    if ctx.operation_error or ctx.operation_failed then return false end
    local accepted = ctx.terminal_settlement or ctx.pending_completion
    if accepted then
      emit_event({
        kind = "mag.terminal_settlement_ignored",
        from = node_id,
        accepted_from = accepted.from,
        reason = "already_settled",
      })
      return false
    end
    local settlement = {
      from = node_id,
      result = result,
      persisted_result = persisted_result,
    }
    if #ctx.operations > 0 or ctx.operation_draining or #ctx.operation_queue > 0 then
      ctx.pending_completion = settlement
    else
      publish_completion(settlement)
    end
    return true
  end

  ctx.settle_quiescent = function()
    if ctx.operation_failed then
      ctx.pending_completion = nil
      ctx.run_complete = nil
      return false
    end
    if ctx.pending_completion and not ctx.operation_draining and #ctx.operation_queue == 0 then
      local settlement = ctx.pending_completion
      ctx.pending_completion = nil
      publish_completion(settlement)
      return true
    end
    return false
  end

  -- Actor and junction outputs share terminal and operation observation. Only
  -- actor routing performs per-actor persistence/lifecycle publication.
  local function observe_typed_output(endpoint, wire, output)
    if ctx.operation_failed or ctx.run_failed then return false end
    local boundary = ctx.result_boundary
    local dynamic = output.dynamic
    local terminal = boundary and boundary.endpoint.constructor == endpoint.constructor
      and boundary.endpoint.value.id == endpoint.value.id and boundary.wire == wire
      and (dynamic == nil or dynamic.kind == "complete")
    if terminal then
      if not nefor.semantic_type.accepts(boundary.type, output.semantic_type) then
        emit_event({kind="mag.run_failed",from=endpoint.value.id,failure="typed-result",
          error="result semantic type is incompatible"})
        return false
      end
      if not settle_result(endpoint.value.id, output) then return false end
    end
    ctx.emission_seq = ctx.emission_seq + 1
    for _, operation in ipairs(ctx.operations) do
      local on = operation.on
      if on.endpoint.constructor == endpoint.constructor and on.endpoint.value.id == endpoint.value.id
          and on.wire == wire and nefor.semantic_type.accepts(on.type, output.semantic_type) then
        local value = output.semantic_value
        if value == nil then value = output.value end
        ctx.operation_queue[#ctx.operation_queue + 1] = {
          operation=operation, emission_seq=ctx.emission_seq,
          source={endpoint=plain_data.copy(endpoint),wire=wire}, value=plain_data.copy(value),
        }
      end
    end
    return true, terminal
  end

  -- Injected host bus seam. Routing already mints run-scoped capability ids.
  local function bus_emit(envelope)
    if type(envelope) == "table" then nefor.emit(envelope) end
  end

  -- The registry is injected so the fold validates every modification's
  -- routes against factory-declared contracts at apply (inventory.lua,
  -- validate_routes) — a route no destination port accepts REJECTS the
  -- modification instead of warn-dropping at delivery.
  local inv = inventory.new({ log = log, registry = registry })

  -- Capability correlation ids are minted scope-prefixed (`r3/cap-7`) so two
  -- concurrent runs' requests stay distinct on the shared bus and an inbound
  -- tool.result dispatches to exactly one context (bus_response below tries
  -- each run; unique ids make at most one claim it).
  local cap_seq = 0
  local topo
  local router = routing.new({
    inventory = inv,
    registry = registry,
    log = log,
    bus_emit = bus_emit,
    invocation_provenance = function(actor_id, capability_id, request)
      local provenance = {
        session_id = ctx.session_id,
        run_id = ctx.run_id,
        run_scope = ctx.scope,
        actor_id = actor_id,
        capability_id = capability_id,
        principal = ctx.principal,
        conversation_id = ctx.actor_conversations[actor_id],
        root_conversation_id = ctx.conversation_id,
      }
      if type(request) == "table" then
        if type(request.provider) == "string" and request.provider ~= "" then
          provenance.provider = request.provider
        end
        if type(request.model) == "string" and request.model ~= "" then
          provenance.model = request.model
        end
      end
      return provenance
    end,
    events = emit_event,
    persist_output = persist_output,
    settle_result = settle_result,
    deliver_endpoint = function(kind, id, port, arrival)
      if kind == "junction" then
        topo:enqueue(id, port, arrival)
      else
        router:deliver(id, arrival)
      end
    end,
    observe_output = function(actor, wire, output)
      return observe_typed_output({constructor="ActorEndpoint",value={id=actor}}, wire, output)
    end,
    topology_routes = function() return topo.routes end,
    -- Host clock for the busy-window stamps (mag.actor_idle's busy_ms).
    -- nil-safe: routing falls back to a zero clock where the host surface
    -- lacks now_ms (the bare-VM test stub).
    now_ms = nefor.now_ms,
    gen_id = function()
      cap_seq = cap_seq + 1
      return scope .. "/cap-" .. tostring(cap_seq)
    end,
  })

  topo = Topology.new({
    inventory = inv,
    semantic = assert(nefor.semantic_type),
    dispatch = function(kind, id, port, arrival)
      if kind == "junction" then topo:enqueue(id, port, arrival)
      else router:deliver(id, arrival) end
    end,
    observe = function(id, wire, arrival, payload)
      local observed = plain_data.copy(payload)
      observed.semantic_type = arrival.type
      observed.semantic_type_id = arrival.type_id
      observed.constructor_id = arrival.constructor_id
      observed.arrival_id = arrival.arrival_id
      return observe_typed_output({constructor="JunctionEndpoint",value={id=id}}, wire, observed)
    end,
  })

  -- Break the construction-order cycle (the hooks need the router, which needs
  -- the inventory): the kill hook first hands the dying instance its final kill
  -- message (dispatch_kill runs handle_kill; its abort envelopes take the
  -- raw-emit path to the bus) and THEN drops the router's firing slots +
  -- correlations (forget). The order is load-bearing — emit-before-forget — so
  -- a dying actor's provider-cancel reaches the bus while it is still bound. A
  -- kill before construction finds no instance: the spec drops, no courtesy
  -- delivery. The deliver hook routes live-target sends into routing's firing
  -- machines. The construction probe lets the fold's validate reject a
  -- control-plane `mag.ApprovalReply` at an unconstructed target (a reply can
  -- only answer an outstanding request — actor-model.md, The approval
  -- boundary).
  inv.set_on_kill(function(id)
    router:dispatch_kill(id)
    router:forget(id)
  end)
  inv.set_is_constructed(function(id)
    return router:is_constructed(id)
  end)

  -- Lazy-construction hook (routing.lua construct_instance): builds the
  -- instance via the registry at the actor's FIRST satisfied input contract —
  -- never at apply. `deps` carries kernel-injected capabilities
  -- (actor-model.md): the per-node persistence writer (deps.writer), consumed
  -- by the sink and available to any factory that declares a use for it. A
  -- failure return escalates inside routing (mag.run_failed).
  router:set_construct(function(record)
    local emit = router:emitter(record.id)
    local actor_params = record.params or {}
    if has_model_context(record.factory) then
      local effective = {}
      for key, value in pairs(actor_params) do effective[key] = value end
      actor_params = effective
      local profile, profile_error = lower_model_profile(actor_params.model_profile)
      if profile_error then return nil, profile_error end
      actor_params.model_profile = nil
      local effort, effort_error = lower_reasoning_effort(actor_params.reasoning_effort)
      if effort_error then return nil, effort_error end
      actor_params.reasoning_effort = effort
      local snapshot = ctx.model_snapshot
      local selected = snapshot
      if profile ~= nil then
        if snapshot == nil then
          return nil, "model profile " .. string.format("%q", profile)
            .. " requires a run model_snapshot"
        end
        selected = type(snapshot.profiles) == "table" and snapshot.profiles[profile] or nil
        if selected == nil then
          return nil, "model profile " .. string.format("%q", profile)
            .. " is absent from the run model_snapshot"
        end
      end
      if selected ~= nil then
        actor_params.provider = selected.provider
        actor_params.model = selected.model
        actor_params.reasoning_effort = selected.reasoning_effort
        actor_params.provider_options = selected.provider_options
      end
    end
    local explicit_conversation_id = type(actor_params.conversation_id) == "string"
        and actor_params.conversation_id ~= "" and actor_params.conversation_id or nil
    local conversation_peer = type(actor_params.conversation_peer) == "string"
        and actor_params.conversation_peer ~= "" and actor_params.conversation_peer or nil
    actor_params.conversation_peer = nil
    local peer_conversation_id = conversation_peer
      and ctx.actor_conversations[conversation_peer] or nil
    if conversation_peer and peer_conversation_id == nil then
      return nil, "conversation peer " .. string.format("%q", conversation_peer)
        .. " must be constructed before " .. string.format("%q", record.id)
    end
    local actor_conversation_id = explicit_conversation_id or peer_conversation_id
      or tostring(ctx.conversation_id) .. "/turns/" .. tostring(ctx.run_id)
        .. "/actors/" .. tostring(record.id)
    ctx.actor_conversations[record.id] = actor_conversation_id
    local is_root_conversation = actor_conversation_id == ctx.conversation_id
    local explicit_turn_id = type(actor_params.turn_id) == "string"
        and actor_params.turn_id ~= "" and actor_params.turn_id or nil
    local actor_turn_id = explicit_turn_id
      or (is_root_conversation and ctx.run_id or (ctx.run_id .. "/" .. tostring(record.id)))
    local deps = {
      persistence_owned_by_kernel = true,
      writer = function(output)
        return persist_output(record.id, output)
      end,
      conversation = {
        id = actor_conversation_id,
        root_id = ctx.conversation_id,
        is_root = is_root_conversation,
        turn_id = actor_turn_id,
        provenance = {
          session_id = ctx.session_id,
          run_id = ctx.run_id,
          root_conversation_id = ctx.conversation_id,
          actor_id = record.id,
          factory = record.factory,
        },
        emit = function(fact)
          nefor.emit({ kind = "conversation.fact.append", fact = fact })
        end,
      },
      diagnostic = function(diagnostic)
        diagnostic = diagnostic or {}
        local owned, diagnostic_error = plain_data.owned(diagnostic, "diagnostic")
        if not owned then
          log.warn("diagnostic dropped: " .. tostring(diagnostic_error))
          return false
        end
        emit_event({
          kind = "mag.diagnostic",
          from = record.id,
          diagnostic = owned,
        })
        return true
      end,
    }
    return registry:construct(record.factory, record.id, actor_params, emit, deps)
  end)
  inv.set_deliver(function(to, from, content, message)
    message = message or { content = content }
    router:deliver_initial(to, from, message)
  end)

  -- Observability: the observer wraps apply, deriving lifecycle events and one
  -- ordered modification-log entry from the fold boundary (observer.lua). The
  -- inventory itself stays pure; this is the composition layer.
  local mlog = modlog.new({ persist = persist_modlog_entry })
  local obs = observer.new({ inventory = inv, emit_event = emit_event, modlog = mlog })

  ctx.inventory = inv
  ctx.router = router
  ctx.topology = topo
  ctx.drain_topology = function()
    local ok, err = topo:drain()
    if not ok then
      ctx.pending_completion, ctx.run_complete = nil, nil
      emit_event({kind="mag.run_failed",failure="topology",error=tostring(err),from="mag.topology"})
    end
    return ok, err
  end
  ctx.modlog = mlog
  ctx.observer = obs
  ctx.drain_operations = function()
    if ctx.operation_draining or ctx.operation_failed then return not ctx.operation_failed end
    ctx.operation_draining = true
    while not ctx.operation_failed and #ctx.operation_queue > 0 do
      local trigger = table.remove(ctx.operation_queue, 1)
      local materialized, delta, materialize_error = pcall(operations.materialize, trigger.operation, trigger.value)
      if not materialized then materialize_error, delta = delta, nil end
      if not delta then
        ctx.operation_error = string.format("operation %q materialization failed: %s",
          trigger.operation.id, tostring(materialize_error))
      else
        local outcome = apply_typed_delta(ctx, delta)
        if not outcome.ok then
          ctx.operation_error = string.format("operation %q delta rejected: %s",
            trigger.operation.id, tostring(outcome.error or "unknown rejection"))
        end
      end
      if ctx.operation_error then
        ctx.operation_failed = true
        ctx.operation_queue = {}
        ctx.pending_completion = nil
        ctx.run_complete = nil
        ctx.run_failed = { error = ctx.operation_error, failure = "operation", from = "mag.operation" }
      end
    end
    ctx.operation_draining = false
    ctx.settle_quiescent()
    return not ctx.operation_failed
  end
  -- Exposed so init-level control ops (interrupt_run) can emit run-scoped
  -- lifecycle events through the same run_id-stamping sink the observer uses.
  ctx.emit_event = emit_event
  return ctx
end

-- Tear one run context down: kill every live id through the fold — kill
-- handlers run, so a mid-flight llm's provider-cancel envelope reaches the
-- bus and the routing layer forgets per-id state — then drop the context.
-- Dropping is the whole "reset": the next run gets a fresh context, i.e. a
-- fold starting from NullGraph, so ids are freely reusable across runs.
--
-- `reason` names WHY the teardown happens and rides every `mag.actor_killed`
-- the reap emits (observer.lua) — "run_complete" / "run_failed" / "killed" /
-- "reaped" — so consumers can tell a completed run's bookkeeping sweep from a
-- real termination. Mechanics are identical for all reasons.
local function reap_run(run_id, reason)
  local ctx = runs[run_id]
  if not ctx then
    return false
  end
  local leftovers = {}
  for id, record in ctx.inventory.pairs() do
    if record.state == "alive" then
      leftovers[#leftovers + 1] = id
    end
  end
  if #leftovers > 0 then
    table.sort(leftovers)
    ctx.observer:apply({ kills = leftovers }, { kill_reason = reason })
  end
  ctx.topology:clear()
  runs[run_id] = nil
  return true
end

local function context_of(run_id)
  local ctx = runs[run_id]
  if ctx then
    return ctx
  end
  return nil, string.format("unknown run '%s' (not begun, or already ended)", tostring(run_id))
end

local function preflight_topology(modification)
  local inv = inventory.new({registry=registry})
  local topo = Topology.new({inventory=inv,semantic=nefor.semantic_type,
    dispatch=function() end,observe=function() return true end})
  local state, err = topo:preflight(modification)
  if not state then return nil, err end
  local prepared = plain_data.copy(modification)
  topo:install(prepared,state)
  prepared.junctions,prepared.routes,prepared.messages=nil,nil,{}
  for _,message in ipairs(modification.messages or {}) do
    local kind,id=Topology.endpoint(message.to)
    if kind=="actor" then local lowered=plain_data.copy(message); lowered.to=id; prepared.messages[#prepared.messages+1]=lowered end
  end
  local result=inv.apply(prepared)
  if not result.ok then return nil,result.error end
  return true
end

local function logical_path_key(path)
  local parts = {}
  for _, name in ipairs(path) do
    parts[#parts + 1] = tostring(#name) .. ":" .. name
  end
  return table.concat(parts, "/")
end

local function validate_logical_nodes(ctx, modification)
  local nodes = modification and modification.nodes
  if nodes == nil then return true end
  if type(nodes) ~= "table" then return nil, "nodes must be a list" end

  local actors = {}
  for _, actor in ipairs(modification.actors or {}) do
    if type(actor) == "table" and type(actor.id) == "string" then actors[actor.id] = 0 end
  end
  local paths = {}
  for key in pairs(ctx.logical_paths or {}) do paths[key] = true end
  for index, node in ipairs(nodes) do
    if type(node) ~= "table" or type(node.path) ~= "table" or #node.path == 0
        or type(node.members) ~= "table" then
      return nil, string.format("nodes[%d] needs a non-empty path and members list", index)
    end
    for segment_index, name in ipairs(node.path) do
      if type(name) ~= "string" or name == "" then
        return nil, string.format("nodes[%d].path[%d] must be a non-empty string",
          index, segment_index)
      end
    end
    local key = logical_path_key(node.path)
    if paths[key] then
      return nil, "duplicate logical node path " .. table.concat(node.path, "/")
    end
    paths[key] = true
    for member_index, actor_id in ipairs(node.members) do
      if type(actor_id) ~= "string" or actor_id == "" then
        return nil, string.format("nodes[%d].members[%d] must be a non-empty string",
          index, member_index)
      elseif (ctx.logical_actors or {})[actor_id] then
        return nil, string.format("actor %q already belongs to logical node %s",
          actor_id, table.concat(ctx.logical_actors[actor_id], "/"))
      elseif actors[actor_id] == nil then
        return nil, string.format("logical node %s references unknown actor %q",
          table.concat(node.path, "/"), tostring(actor_id))
      end
      actors[actor_id] = actors[actor_id] + 1
      if actors[actor_id] > 1 then
        return nil, string.format("actor %q belongs to more than one logical node", actor_id)
      end
    end
  end
  for actor_id, owners in pairs(actors) do
    if owners == 0 then
      return nil, string.format("actor %q has no logical node", actor_id)
    end
  end
  for _, node in ipairs(nodes) do
    if #node.path > 1 then
      local parent = {}
      for index = 1, #node.path - 1 do parent[index] = node.path[index] end
      if not paths[logical_path_key(parent)] then
        return nil, "logical node parent is missing for " .. table.concat(node.path, "/")
      end
    end
  end
  return true
end

apply_with_logical_nodes = function(ctx, modification, opts)
  local valid, validation_error = validate_logical_nodes(ctx, modification)
  if not valid then
    local rejected = { ok = false, error = validation_error }
    return ctx.observer:observe(
      modification or {}, ctx.observer:snapshot(modification or {}), rejected, opts)
  end
  local prospective, topology_error = ctx.topology:preflight(modification)
  if not prospective then
    local rejected = {ok=false,error=topology_error}
    return ctx.observer:observe(modification or {},ctx.observer:snapshot(modification or {}),rejected,opts)
  end
  local snapshot = ctx.topology:snapshot()
  ctx.topology:install(modification, prospective)
  local actor_modification = plain_data.copy(modification)
  actor_modification.junctions, actor_modification.routes, actor_modification.messages = nil, nil, {}
  for _, message in ipairs(modification.messages or {}) do
    local endpoint = message.to and message.to.endpoint
    if endpoint and endpoint.constructor == "ActorEndpoint" then
      local lowered = plain_data.copy(message)
      lowered.to = endpoint.value.id
      actor_modification.messages[#actor_modification.messages + 1] = lowered
    end
  end
  local apply_opts = {}
  for key, value in pairs(opts or {}) do apply_opts[key] = value end
  apply_opts.before_execute = function()
    ctx.observer:nodes_declared(modification.nodes)
    if opts and opts.before_execute then opts.before_execute() end
  end
  local result = ctx.observer:apply(actor_modification, apply_opts)
  if not result.ok then ctx.topology:restore(snapshot); return result end
  for _, message in ipairs(modification.messages or {}) do
    local endpoint = message.to and message.to.endpoint
    if endpoint and endpoint.constructor == "JunctionEndpoint" then ctx.topology:initial(message) end
  end
  local drained = ctx.drain_topology()
  if not drained then
    -- Admission validated every authored junction payload before actor effects.
    -- A later evaluation failure can only come from runtime actor output or an
    -- internal invariant violation, so it is a run failure, not a rejected
    -- modification whose already-visible effects could be rolled back.
    return result
  end
  for _, node in ipairs(modification.nodes or {}) do
    ctx.logical_paths[logical_path_key(node.path)] = true
    for _, actor_id in ipairs(node.members or {}) do ctx.logical_actors[actor_id] = node.path end
  end
  return result
end

apply_typed_delta = function(ctx, mod)
  if type(mod) ~= "table" then
    return { ok = false, error = "a typed run requires a delta object" }
  end
  if mod.result ~= nil then
    return { ok = false, error = "a delta cannot define or replace the result boundary" }
  end
  if type(mod.types) ~= "table" then
    return { ok = false, error = "a typed run requires delta semantic declarations" }
  end
  local semantic_host = nefor and nefor.semantic_type
  local declarations_ok, declarations_result = pcall(
    semantic_host and semantic_host.validate_declarations, mod.types)
  if not declarations_ok or declarations_result ~= true then
    return { ok = false, error = "delta semantic declarations are invalid: "
      .. tostring(declarations_result) }
  end
  local function declared(descriptor, id)
    if type(descriptor) ~= "table" or type(id) ~= "string" or mod.types[id] == nil then return false end
    local ok, actual = pcall(semantic_host.id, descriptor)
    return ok and actual == id
  end
  for _, junction in ipairs(mod.junctions or {}) do
    for _, port in ipairs(junction.inputs or {}) do
      if not declared(port.type, port.type_id) then return {ok=false,error="junction input requires delta semantic declarations"} end
    end
    for _, port in ipairs(junction.outputs or {}) do
      if not declared(port.type, port.type_id) then return {ok=false,error="junction output requires delta semantic declarations"} end
    end
  end
  for _, message in ipairs(mod.messages or {}) do
    if not declared(message.semantic_type, message.semantic_type_id)
        or not declared(message.to and message.to.type, message.to and message.to.type_id) then
      return {ok=false,error="message requires delta semantic declarations"}
    end
    if type(message.content) == "table" and message.content.kind == "mag.ApprovalReply" then
      if type(message.semantic_type) ~= "table"
          or type(message.semantic_type_id) ~= "string"
          or mod.types[message.semantic_type_id] == nil then
        return { ok = false, error = "mag.ApprovalReply requires delta semantic declarations" }
      end
      local validation = type(semantic_host) == "table"
          and type(semantic_host.validate_value) == "function"
          and semantic_host.validate_value(message.semantic_type, message.content)
      if type(validation) ~= "table" or validation.ok ~= true then
        return { ok = false, error = "mag.ApprovalReply has a malformed typed payload" }
      end
    end
  end
  local prepared = plain_data.copy(mod)
  for _, spec in ipairs(prepared.actors or {}) do spec.semantic_strict = true end
  return apply_with_logical_nodes(ctx, prepared, {
    before_execute = function()
      ctx.router:register_type_declarations(prepared.types)
    end,
  })
end

nefor.log("mag-kernel ready")

return {
  name = "mag-kernel",

  -- Begin a run: create its context, then emit mag.run_started before the
  -- first modification. Run identity is injected, never ambient. Returns { ok = true, reaped = {...} } or
  -- { ok = false, error } — a duplicate live run_id rejects (the id is the
  -- context key and the reply correlation; two runs may not share it).
  --
  -- Session-boundary reaping: the engine and this long-lived kernel outlive
  -- TUI sessions, so a run context whose run never terminated in a PREVIOUS
  -- session would leak actors forever. Beginning a run under a new session_id
  -- reaps every live context from a different session — the per-run analogue
  -- of the old global kill-sweep, scoped so concurrent runs of the CURRENT
  -- session are never touched. Reaped run_ids are returned so the host can
  -- fail their still-pending execute replies.
  begin_run = function(meta)
    meta = meta or {}
    if type(meta.run_id) ~= "string" or meta.run_id == "" then
      return { ok = false, error = "begin_run requires a string run_id" }
    end
    if runs[meta.run_id] then
      return { ok = false, error = string.format("run '%s' is already live", meta.run_id) }
    end
    local stale = {}
    for run_id, ctx in pairs(runs) do
      if ctx.session_id ~= meta.session_id then
        stale[#stale + 1] = run_id
      end
    end
    table.sort(stale)
    for _, run_id in ipairs(stale) do
      reap_run(run_id, "reaped")
    end
    local ctx = new_run_context(meta)
    runs[meta.run_id] = ctx
    -- The scope token rides run_started so the run's spawner can bind
    -- prefix-scoped wire ids (chat handles, correlation ids) to this run
    -- without parsing them (observer.lua run_started).
    ctx.observer:run_started({
      run_id = ctx.run_id,
      run_name = ctx.run_name,
      session_id = ctx.session_id,
      scope = ctx.scope,
      principal = ctx.principal,
    })
    return { ok = true, reaped = stale }
  end,

  -- Validate the complete immutable program before a run exists. This is also
  -- used by mag.load, so an artifact remains executable after its compiler
  -- source has been discarded.
  preflight_program = function(initial, program_operations)
    local checked, err = operations.preflight(initial, program_operations or {}, registry)
    if not checked then return { ok = false, error = err } end
    local logical_ok, logical_error = validate_logical_nodes({logical_paths={},logical_actors={}}, initial)
    if not logical_ok then return {ok=false,error=logical_error} end
    local valid, topology_error = preflight_topology(initial)
    if not valid then return {ok=false,error=topology_error} end
    return { ok = true }
  end,

  -- Start a run's program: apply its initial modification through that run's
  -- fold. Spawns register specs, initial messages deliver immediately
  -- (registration already put every route and input contract in place), and
  -- each actor constructs lazily at its first satisfied input contract
  -- (docs/ir.md, Running a program). Applied through the observer-wrapped
  -- apply, so modification #0 is recorded in the run's modlog and its
  -- lifecycle events fire. The context is fresh from begin_run — starting IS
  -- starting from NullGraph; no sweep, and a run starting mid-another-run
  -- touches nothing outside its own context. Returns the fold's verbatim
  -- { ok = true } | { ok = false, error = "..." }.
  start = function(run_id, mod, program_operations)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    local checked, operation_error = operations.preflight(mod, program_operations or {}, registry)
    if not checked then return { ok = false, error = operation_error } end
    ctx.operations = checked
    local boundary = mod and mod.result and mod.result.from
    local endpoint = type(boundary) == "table" and boundary.endpoint
    local endpoint_value = type(endpoint) == "table" and endpoint.value
    if type(endpoint) ~= "table" or type(endpoint_value) ~= "table"
        or (endpoint.constructor ~= "ActorEndpoint" and endpoint.constructor ~= "JunctionEndpoint")
        or type(endpoint_value.id) ~= "string" or endpoint_value.id == ""
        or type(boundary.wire) ~= "string" or boundary.wire == "" then
      return { ok = false, error = "initial artifact needs endpoint-addressed result.from" }
    end
    local typed_artifact = type(mod.types) == "table" and next(mod.types) ~= nil
    ctx.semantic_strict = typed_artifact
    if typed_artifact and (type(boundary.type_id) ~= "string" or type(boundary.type) ~= "table") then
      return { ok = false, error = "typed initial artifact needs a typed result.from port" }
    end
    local source
    local definitions = endpoint.constructor == "ActorEndpoint" and (mod.actors or {}) or (mod.junctions or {})
    for _, definition in ipairs(definitions) do if definition.id == endpoint_value.id then source = definition break end end
    if not source then
      return {ok=false,error=string.format("result boundary source %s %q does not exist",
        endpoint.constructor, endpoint_value.id)}
    end
    local declared = false
    for _, output in ipairs(source.outputs or {}) do
      if output.wire == boundary.wire and (not typed_artifact or output.type_id == boundary.type_id) then
        declared = not typed_artifact or (nefor.semantic_type.id(boundary.type) == boundary.type_id
          and nefor.semantic_type.id(output.type) == output.type_id)
        if declared then break end
      end
    end
    if not declared then
      return {ok=false,error=string.format("result boundary %q is not a declared output of %s %q",
        boundary.wire, endpoint.constructor, endpoint_value.id)}
    end
    ctx.result_boundary = plain_data.copy(boundary)
    ctx.router:set_result_boundary(boundary)
    if typed_artifact then ctx.router:register_type_declarations(mod.types) end
    local modification = {}
    for key, value in pairs(mod) do
      if key ~= "result" and (typed_artifact or key ~= "types") then
        modification[key] = value
      end
    end
    if typed_artifact then
      for _, spec in ipairs(modification.actors or {}) do
        spec.semantic_strict = true
      end
    end
    local outcome = apply_with_logical_nodes(ctx, modification)
    if outcome.ok then ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology() end
    return outcome
  end,

  -- Apply one graph modification through a run's fold. Strictly serialized
  -- within the run — one call, one modification (docs/ir.md).
  apply = function(run_id, mod)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    local outcome = apply_typed_delta(ctx, mod)
    if outcome.ok then ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology() end
    return outcome
  end,


  -- Drain one actor gracefully within a run (actor-model.md, Signals: drain /
  -- SIGTERM): calls its handle_drain where declared. This is the graceful path
  -- and is never auto-invoked from kill; removal, when it comes, is a separate
  -- kill in a modification. Returns true when a drain handler ran.
  drain = function(run_id, id)
    local ctx = runs[run_id]
    if not ctx then
      return false
    end
    local handled = ctx.router:drain(id)
    if handled then ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology() end
    return handled
  end,

  steer_run = function(run_id, id, message)
    local ctx = runs[run_id]
    if not ctx then return false end
    return ctx.router:steer(id, message)
  end,

  resume_actor = function(run_id, id, message)
    local ctx = runs[run_id]
    if not ctx then return false end
    local record = ctx.inventory.get(id)
    if not record or record.state ~= "alive" then return false end
    ctx.router:activate(id, {
      messages = { { from = "mag.owner-result", message = message } },
    })
    ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology()
    return true
  end,

  -- Interrupt a live run's in-flight work. Two shapes, selected by `terminate`:
  --
  -- GRACEFUL (`terminate` falsy — the lead's OWN turn): settle every in-flight
  -- capability correlation as a failed reply "interrupted by user" and emit a
  -- `tool.cancel` for each so the real work stops (routing.lua interrupt); the
  -- failure routes through the normal tool-failure path (run-tool → tool-result
  -- → llm re-fire), so the run STAYS ALIVE and winds down to a real final
  -- answer — the no-amnesia path. The host does not end the run; it settles on
  -- its own completion. Returns { ok = true, interrupted = <count> }.
  -- Terminating sub-runs bypass this graceful path: the Rust runtime reaps
  -- their actors directly, and actor teardown owns exactly one cancellation
  -- per open correlation before the failed run result.
  interrupt_run = function(run_id, failure)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    local settled = ctx.router:interrupt(failure)
    ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology()
    -- Observable interrupt marker for the panel/transcript (run_id-stamped by
    -- the sink). Not a kill: no actor_killed, the run continues.
    ctx.emit_event({
      kind = "mag.run_interrupted",
      interrupted = settled,
    })
    return { ok = true, interrupted = settled }
  end,

  -- End a run: reap its live actors through the fold (kill handlers run —
  -- abort/cancel envelopes reach the bus) and drop the context. The host calls
  -- this once the run settled (complete / failed); killing a run outright is
  -- the same call. `reason` stamps the teardown's `mag.actor_killed` events
  -- ("run_complete" / "run_failed" / "killed"); absent, the teardown is an
  -- outright kill. Returns true when a context existed.
  end_run = function(run_id, reason)
    return reap_run(run_id, reason or "killed")
  end,

  -- The registered factory names — the control plane validates reasoner/factory
  -- types against this instead of a hand-synced allowlist. Source of truth is
  -- the registry (registry.lua).
  registry_names = function()
    return registry:names()
  end,

  -- Plain-data factory contracts supplied to MAG compilation as immutable
  -- input. Qualified identity is the authored/lowered name; implementation is
  -- retained only so the runtime can bind it to the registered constructor.
  registry_contracts = function(array_mt)
    return registry:contracts(array_mt)
  end,

  -- Take a run's run-complete signal (one-shot; cleared on read). The host
  -- polls this after driving the fold to settle the execute reply with the
  -- sink's output PATH. Returns nil until the run signals completion.
  take_run_complete = function(run_id)
    local ctx = runs[run_id]
    if not ctx then
      return nil
    end
    if ctx.run_complete_taken then return nil end
    local rc = ctx.run_complete
    if rc then ctx.run_complete_taken = true end
    return rc
  end,

  -- Take a run's unhandled-failure signal (one-shot; cleared on read). Set
  -- when a failed completion's tag routes nowhere (routing.lua
  -- apply_completion → mag.run_failed). The host fails the run with the
  -- carried error detail. Returns nil while no failure escalated.
  take_run_failed = function(run_id)
    local ctx = runs[run_id]
    if not ctx then
      return nil
    end
    local rf = ctx.run_failed
    ctx.run_failed = nil
    return rf
  end,

  bus_observation = function(observation)
    for run_id, ctx in pairs(runs) do
      if ctx.router:bus_observation(observation) then return run_id end
    end
    return nil
  end,

  -- Deliver a correlated capability response (tool.result-shaped:
  -- { id, result | error }) back to the requesting actor. Correlation ids are
  -- scope-prefixed and kernel-unique, so trying each live run finds at most
  -- one owner; the owning run's id is returned (nil when the id is not ours —
  -- another consumer's reply on the broadcast bus). The host uses the return
  -- to settle exactly the run the response advanced.
  bus_response = function(response)
    for run_id, ctx in pairs(runs) do
      if ctx.router:bus_response(response) then
        ctx.drain_topology(); ctx.drain_operations(); ctx.drain_topology()
        return run_id
      end
    end
    return nil
  end,

  -- The live run ids, sorted (host/tests iteration).
  run_ids = function()
    local ids = {}
    for run_id in pairs(runs) do
      ids[#ids + 1] = run_id
    end
    table.sort(ids)
    return ids
  end,

  -- Read-only introspection for the host / tests, per run.
  state_of = function(run_id, id)
    local ctx = runs[run_id]
    if not ctx then
      return "never-existed"
    end
    return ctx.inventory.state_of(id)
  end,
  actor = function(run_id, id)
    local ctx = runs[run_id]
    if not ctx then
      return nil
    end
    return ctx.inventory.get(id)
  end,

  -- A run's whole context — inventory, router, observer, modlog ("the
  -- modification log is the run"; docs/ir.md), scope token — for tests and
  -- host wiring that need more than the seams above. nil once the run ended.
  context = function(run_id)
    return runs[run_id]
  end,

  -- The shared factory registry (read-only after seeding).
  registry = registry,
}
