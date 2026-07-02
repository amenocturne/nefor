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

-- Shared per-node output persistence (lua/libs/output-persistence). The mag
-- plugin host currently exposes only nefor.log (plugins/mag/src/kernel.rs,
-- install_nefor) and points package.path at the kernel directory alone, so this
-- require degrades to nil there and persistence becomes a no-op. Expose the lib
-- on package.path plus nefor.fs/json/sessions on the host to activate it.
local ok_persist, persistence = pcall(require, "output-persistence")
if not ok_persist then
  persistence = nil
end

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
  return reg
end

nefor.log("mag-kernel loading")

local registry = build_registry()
local inv = inventory.new({ log = make_logger() })

-- Host-provided run context (session/run ids). Kept injected — the kernel
-- modules never reach for ambient run identity; begin_run threads it in.
local run_ctx = { run_id = nil, run_name = nil }

-- Injected lifecycle-event sink (observer.lua's EVENTS set, plus routing's
-- ready/run-complete). No host event-bus surface exists yet (the mag plugin
-- ships only nefor.log), so events are traced and dropped until it lands — swap
-- this for the real host broadcast when it exists. Flagged; mirrors bus_emit.
local function emit_event(event)
  nefor.log(string.format("[event] %s %s",
    tostring(event and event.kind),
    tostring(event and (event.id or event.from or ""))))
end

-- Per-node output persistence keyed by actor id, reusing output-persistence's
-- session layout (sessions/<id>/mag/runs/<run>/<node>.output). Degrades to a
-- no-op until the host exposes the lib + nefor.fs/json/sessions.
local function persist_output(node_id, output)
  if not persistence then
    return
  end
  persistence.persist(
    { run_id = run_ctx.run_id, run_name = run_ctx.run_name, node_id = node_id },
    output)
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

-- Injected host bus seam. The mag plugin's bus surface (tool.invoke out,
-- tool.result in) is not wired yet (plugins/mag/src/kernel.rs install_nefor
-- ships only nefor.log). Until it lands, capability requests are logged and
-- dropped rather than reaching a plugin; swap this stub for the real host
-- bus emit when it exists. Kept dependency-injected so the kernel modules
-- stay pure (routing.lua, correlation.lua).
local function bus_emit(envelope)
  nefor.log(string.format("[bus-seam] capability request to '%s' (id=%s) dropped: host bus not wired",
    tostring(envelope and envelope.name), tostring(envelope and envelope.id)))
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
-- the inventory): the kill hook drops the router's firing slots + correlations;
-- the construct hook builds each freshly-spawned instance via the registry and
-- binds it into the router; the deliver hook routes live-target sends through
-- routing's single fire-vs-queue decision.
inv.set_on_kill(function(id)
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

-- Injected clock (flagged): the host has no time binding yet (kernel.rs
-- install_nefor ships only nefor.log), so the barrier reads os.time at
-- second granularity. Swap for a host-provided millisecond clock when it
-- lands — time stays injected, never read inside the pure modules.
local function now_ms()
  return os.time() * 1000
end

-- Observability: the observer wraps apply, deriving lifecycle events and one
-- ordered modification-log entry from the fold boundary (observer.lua). The
-- inventory itself stays pure; this is the composition layer.
local obs = observer.new({ inventory = inv, emit_event = emit_event, modlog = mlog })

nefor.log("mag-kernel ready (factories: stub, loop-counter, sink, human)")

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
  -- readies arrive. `o.deadline_ms` overrides the per-program default.
  -- NOTE: the barrier applies the initial modification through the inventory
  -- directly, bypassing the observer — the initial modification is not yet
  -- modlogged (reconciliation pending; single composition point).
  start = function(mod, o)
    o = o or {}
    return barrier.start({
      inventory = inv,
      router = router,
      mod = mod,
      now = o.now or now_ms,
      deadline_ms = o.deadline_ms,
    })
  end,

  -- Advance a pending barrier handle against the host clock (ms).
  poll = function(handle, t)
    return barrier.poll(handle, t or now_ms())
  end,

  -- Begin a run: set the host-provided run context (session/run ids) and emit
  -- mag.run_started before the first modification (parity with reasoner-graph's
  -- RunStarted). Run identity is injected, never ambient.
  begin_run = function(meta)
    meta = meta or {}
    run_ctx.run_id = meta.run_id or run_ctx.run_id
    run_ctx.run_name = meta.run_name or run_ctx.run_name
    obs:run_started({ run_id = run_ctx.run_id, run_name = run_ctx.run_name })
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
