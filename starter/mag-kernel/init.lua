-- starter/mag-kernel/init.lua — MAG actor-kernel entry.
--
-- Loaded by the `mag` plugin's embedded Lua VM at startup. This module is
-- the wiring layer: it adapts the host `nefor` surface into the plain
-- dependencies the kernel modules expect, builds the factory registry (the
-- trait layer composition validates against — see registry.lua, shape.lua),
-- constructs the inventory (which owns the fold over graph modifications —
-- see inventory.lua, plugins/mag/docs/actor-model.md, docs/ir.md), and
-- returns the kernel table the plugin holds for the session.
--
-- `nefor.log` is a host binding that writes to the plugin's tracing
-- subscriber (stderr). The kernel must never write to stdout — that is the
-- NCP wire. The host sets `package.path` to this directory before loading,
-- so sibling modules resolve by bare name (`require("inventory")`).

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local barrier = require("barrier")
local modlog = require("modlog")
local observer = require("observer")
local stub = require("factories.stub")
local loop_counter = require("factories.loop-counter")
local sink = require("factories.sink")
local human = require("factories.human")
local llm = require("factories.llm")

-- Shared per-node output persistence (lua/libs/output-persistence). The mag
-- plugin host currently exposes only nefor.log (plugins/mag/src/kernel.rs,
-- install_nefor) and points package.path at the kernel directory alone, so this
-- require degrades to nil there and persistence becomes a no-op. Expose the lib
-- on package.path plus nefor.fs/json/sessions on the host to activate it.
local ok_persist, persistence = pcall(require, "output-persistence")
if not ok_persist then
  persistence = nil
end
local run_tool = require("factories.run-tool")
local tool_result = require("factories.tool-result")
local adapter = require("factories.adapter")

-- Adapt the host's single `nefor.log(msg)` function into the leveled sink
-- the kernel modules expect. Level travels as a prefix so the one host
-- binding covers info/warn/error without a wider native surface.
local function make_logger()
  local function at(level)
    return function(msg)
      nefor.log(string.format("[%s] %s", level, msg))
    end
  end
  return { info = at("info"), warn = at("warn"), error = at("error") }
end

-- Build the registry and seed the factories shipped with the kernel.
local function build_registry()
  local reg = Registry.new()
  local function seed(mod)
    local _, err = reg:register({ declaration = mod.declaration, construct = mod.construct })
    if err then
      error("mag-kernel: failed to register factory '" .. tostring(mod.declaration and mod.declaration.name) .. "': " .. err)
    end
  end
  seed(stub)
  seed(loop_counter)
  seed(sink)
  seed(human)
  seed(llm)
  seed(run_tool)
  seed(tool_result)
  seed(adapter)
  return reg
end

nefor.log("mag-kernel loading")

local registry = build_registry()
local inv = inventory.new({ log = make_logger() })

-- Host-provided run context (session/run ids) plus the two host-facing outputs
-- the control plane collects: the last sink output path (surfaced on the
-- run-complete event and via take_run_complete) and the last run-complete
-- signal itself. Kept injected — the kernel modules never reach for ambient run
-- identity; begin_run threads it in.
local run_ctx = {
  run_id = nil,
  run_name = nil,
  session_id = nil,
  last_output_path = nil,
  run_complete = nil,
  run_failed = nil,
}

-- Injected lifecycle-event sink (observer.lua's EVENTS set, plus routing's
-- ready/run-complete). Every lifecycle event is broadcast on the NCP bus as a
-- Body::Event (via the host's nefor.emit queue, drained by the plugin after
-- each kernel call). The run-complete event additionally carries the sink's
-- persisted output PATH (control plane reads paths, never node data) and is
-- captured so the host can settle the execute reply.
local function emit_event(event)
  if type(event) ~= "table" then
    return
  end
  if event.kind == observer.EVENTS.run_complete then
    -- Attach the path the sink's writer persisted this run (recorded by
    -- persist_output below) and stash the signal for take_run_complete.
    event.output_path = run_ctx.last_output_path
    run_ctx.run_complete = {
      output_path = run_ctx.last_output_path,
      persisted = event.persisted,
    }
  elseif event.kind == observer.EVENTS.run_failed then
    -- An unhandled actor failure (routing.lua apply_completion). Stash it for
    -- take_run_failed so the host fails the run with the detail surfaced.
    run_ctx.run_failed = {
      error = event.error,
      failure = event.failure,
      from = event.from,
    }
  end
  nefor.emit(event)
end

-- Per-node output persistence keyed by actor id, reusing output-persistence's
-- session layout (sessions/<id>/mag/runs/<run>/<node>.output). The host now
-- exposes nefor.fs/json and puts the lib on package.path; the session id is
-- injected through run_ctx (the plugin has no resident sessions actor).
-- Records the written path so the run-complete event can carry it.
local function persist_output(node_id, output)
  if not persistence then
    return
  end
  local persisted = persistence.persist(
    {
      run_id = run_ctx.run_id,
      run_name = run_ctx.run_name,
      session_id = run_ctx.session_id,
      node_id = node_id,
    },
    output)
  if type(persisted) == "table" and type(persisted.output_path) == "string" then
    run_ctx.last_output_path = persisted.output_path
  end
end

-- The sink's construct-time writer seam (factories/sink.lua, deps.writer): the
-- same per-node persistence, bound to one id, threaded through deps by the
-- construction layer via deps_for below.
local function writer_for(node_id)
  return function(output)
    return persist_output(node_id, output)
  end
end

-- Modification-log JSONL sink (one line per entry; docs/ir.md). No host fs/json
-- surface yet, so entries are traced until it lands; the in-memory log is always
-- retained regardless.
local function persist_modlog_entry(entry)
  nefor.log(string.format("[modlog] #%d %s",
    tonumber(entry.seq) or -1, tostring(entry.outcome)))
end
local mlog = modlog.new({ persist = persist_modlog_entry })

-- Injected host bus seam. Routing hands this a tool.invoke-shaped envelope
-- ({ kind = "tool.invoke", id, name, args }; routing.lua on_capability_invoke
-- already performs the capability→tool.invoke rename). We put it straight on
-- the NCP bus via the host's nefor.emit queue. Kept dependency-injected so the
-- kernel modules stay pure (routing.lua, correlation.lua); the capability→wire
-- naming is the composition layer's job, not the pure modules'.
local function bus_emit(envelope)
  if type(envelope) == "table" then
    nefor.emit(envelope)
  end
end

local router = routing.new({
  inventory = inv,
  registry = registry,
  log = make_logger(),
  bus_emit = bus_emit,
  events = emit_event,
  persist_output = persist_output,
})
-- Break the construction-order cycle (both hooks need the router, which needs
-- the inventory): the kill hook first hands the dying instance its final kill
-- message (dispatch_kill runs handle_kill; its abort envelopes take the raw-emit
-- path to the bus) and THEN drops the router's firing slots + correlations
-- (forget). The order is load-bearing — emit-before-forget — so a dying actor's
-- provider-cancel/ApprovalCancel reaches the bus while it is still bound. The
-- construct hook builds each freshly-spawned instance via the registry and binds
-- it into the router; the deliver hook routes live-target sends through routing's
-- single fire-vs-queue decision.
inv.set_on_kill(function(id)
  router:dispatch_kill(id)
  router:forget(id)
end)

local construct_log = make_logger()
inv.set_construct(function(record)
  -- `deps` carries kernel-injected capabilities (actor-model.md): the per-node
  -- persistence writer (deps.writer), consumed by the sink and available to
  -- any factory that declares a use for it.
  local emit = router:emitter(record.id)
  local deps = { writer = writer_for(record.id) }
  local instance, err = registry:construct(record.factory, record.id, record.params, emit, deps)
  if err or not instance then
    -- Construction failed: the instance is not bound and never readies, so the
    -- program-start barrier will name this id as a straggler.
    construct_log.error(string.format("construct failed for '%s' (factory '%s'): %s",
      tostring(record.id), tostring(record.factory), tostring(err)))
    return
  end
  router:bind(record.id, instance)
end)
inv.set_deliver(function(to, from, content)
  content = content or {}
  router:deliver(to, from, content.kind, content)
end)

-- Injected clock: the host provides a millisecond clock (nefor.now_ms); we fall
-- back to os.time at second granularity only if it is somehow absent. Time stays
-- injected, never read inside the pure modules (barrier.lua takes `now`).
local function now_ms()
  if type(nefor.now_ms) == "function" then
    return nefor.now_ms()
  end
  return os.time() * 1000
end

-- Observability: the observer wraps apply, deriving lifecycle events and one
-- ordered modification-log entry from the fold boundary (observer.lua). The
-- inventory itself stays pure; this is the composition layer.
local obs = observer.new({ inventory = inv, emit_event = emit_event, modlog = mlog })

nefor.log("mag-kernel ready (factories: stub, loop-counter, sink, human, llm, run-tool, tool-result, adapter)")

return {
  name = "mag-kernel",

  -- Apply one graph modification through the fold. Strictly serialized —
  -- one call, one modification (docs/ir.md). Routed through the observer so
  -- each apply also emits lifecycle events and records a modification-log
  -- entry; the returned result is the fold's verbatim
  -- { ok = true } or { ok = false, error = "..." }.
  apply = function(mod)
    return obs:apply(mod)
  end,

  -- Start a program: apply its initial modification behind the ready barrier
  -- (spawn all → await readies → deliver initial messages; docs/ir.md). Returns
  -- a barrier handle; if `handle.done` is false the host polls it as late
  -- readies arrive. `o.deadline_ms` overrides the per-program default. The
  -- barrier applies the initial modification through the observer-wrapped apply
  -- (the same path as `apply` below), so modification #0 is recorded in the
  -- modlog and its lifecycle events fire — one composition point, no bypass.
  start = function(mod, o)
    o = o or {}
    -- A program start owns the whole constellation: reset the previous run's
    -- actors before applying the new program. Kill every live id through the
    -- fold (kill handlers run — a mid-flight llm's provider-cancel envelope
    -- reaches the bus — and the routing layer forgets per-id state), then drop
    -- the tombstones so the new program can reuse ids. Without this, a
    -- re-executed program found its ids still alive in the resident kernel:
    -- every spawn degraded to a duplicate-alive no-op — no construct, no
    -- actor_spawned/actor_ready events — and stale instances leaked state
    -- across runs (an llm's request counter continuing at @r3).
    local leftovers = {}
    for id, record in inv.pairs() do
      if record.state == "alive" then
        leftovers[#leftovers + 1] = id
      end
    end
    if #leftovers > 0 then
      table.sort(leftovers)
      obs:apply({ kills = leftovers })
    end
    inv.clear()
    return barrier.start({
      inventory = inv,
      router = router,
      apply = function(m)
        return obs:apply(m)
      end,
      mod = mod,
      now = o.now or now_ms,
      deadline_ms = o.deadline_ms,
    })
  end,

  -- Advance a pending barrier handle against the host clock (ms).
  poll = function(handle, t)
    return barrier.poll(handle, t or now_ms())
  end,

  -- Drain one actor gracefully (actor-model.md, Signals: drain / SIGTERM):
  -- calls its handle_drain where declared. This is the graceful path and is
  -- never auto-invoked from kill; removal, when it comes, is a separate kill in
  -- a modification. Returns true when a drain handler ran.
  drain = function(id)
    return router:drain(id)
  end,

  -- Begin a run: set the host-provided run context (session/run ids) and emit
  -- mag.run_started before the first modification (parity with reasoner-graph's
  -- RunStarted). Run identity is injected, never ambient.
  begin_run = function(meta)
    meta = meta or {}
    run_ctx.run_id = meta.run_id or run_ctx.run_id
    run_ctx.run_name = meta.run_name or run_ctx.run_name
    run_ctx.session_id = meta.session_id or run_ctx.session_id
    -- Fresh run: clear the prior run's terminal captures + persisted path.
    run_ctx.run_complete = nil
    run_ctx.run_failed = nil
    run_ctx.last_output_path = nil
    obs:run_started({ run_id = run_ctx.run_id, run_name = run_ctx.run_name })
  end,

  -- The registered factory names — the control plane validates reasoner/factory
  -- types against this instead of a hand-synced allowlist. Source of truth is
  -- the registry (registry.lua).
  registry_names = function()
    return registry:names()
  end,

  -- Take the last run-complete signal (one-shot; cleared on read). The host
  -- polls this after driving the fold to settle the execute reply with the
  -- sink's output PATH. Returns nil until the resident run signals completion.
  take_run_complete = function()
    local rc = run_ctx.run_complete
    run_ctx.run_complete = nil
    return rc
  end,

  -- Take the last unhandled-failure signal (one-shot; cleared on read). Set
  -- when a failed completion's tag routes nowhere (routing.lua
  -- apply_completion → mag.run_failed). The host fails the run with the
  -- carried error detail. Returns nil while no failure escalated.
  take_run_failed = function()
    local rf = run_ctx.run_failed
    run_ctx.run_failed = nil
    return rf
  end,

  -- Kernel-injected construct deps for an actor id — the sink's per-node
  -- persistence writer (factories/sink.lua, deps.writer). The construction
  -- layer threads this into registry:construct(name, id, params, emit, deps).
  deps_for = function(id)
    return { writer = writer_for(id) }
  end,

  -- The modification log ("the modification log is the run"; docs/ir.md) and a
  -- replay helper folding it back into a caller-supplied fresh inventory.
  modlog = mlog,
  replay = function(fresh_inv)
    return modlog.replay(mlog:all(), fresh_inv)
  end,
  observer = obs,


  -- Read-only introspection for the host / tests.
  state_of = function(id)
    return inv.state_of(id)
  end,
  actor = function(id)
    return inv.get(id)
  end,

  -- Deliver a correlated capability response (tool.result-shaped:
  -- { id, result | error }) back to the requesting actor. The host calls
  -- this from its bus-inbound path once the bus seam is wired.
  bus_response = function(response)
    return router:bus_response(response)
  end,

  -- The inventory, factory registry, and router, for wiring factory
  -- construction and host bus I/O in their own tasks without re-reaching
  -- through this table.
  inventory = inv,
  registry = registry,
  router = router,
}
