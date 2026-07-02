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
local stub = require("factories.stub")
local loop_counter = require("factories.loop-counter")
local sink = require("factories.sink")
local human = require("factories.human")

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
  -- `deps` carries kernel-injected capabilities (actor-model.md). The sink's
  -- output writer is not wired yet (no run output location at this layer), so
  -- deps is empty for now; a sink constructed here flags persisted = false.
  local emit = router:emitter(record.id)
  local instance, err = registry:construct(record.factory, record.id, record.params, emit, {})
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

nefor.log("mag-kernel ready (factories: stub, loop-counter, sink, human)")

return {
  name = "mag-kernel",

  -- Apply one graph modification through the fold. Strictly serialized —
  -- one call, one modification (docs/ir.md). Returns { ok = true } or
  -- { ok = false, error = "..." }.
  apply = function(mod)
    return inv.apply(mod)
  end,

  -- Start a program: apply its initial modification behind the ready barrier
  -- (spawn all → await readies → deliver initial messages; docs/ir.md). Returns
  -- a barrier handle; if `handle.done` is false the host polls it as late
  -- readies arrive. `o.deadline_ms` overrides the per-program default.
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
