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
local stub = require("factories.stub")
local sink = require("factories.sink")
local human = require("factories.human")
local llm = require("factories.llm")
local structured_output = require("factories.structured-output")

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
local bash = require("factories.bash")

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
  seed(human)
  seed(llm)
  seed(structured_output)
  seed(run_tool)
  seed(tool_result)
  seed(adapter)
  seed(bash)
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

-- Build one run context. `meta` carries the host-provided run identity
-- (run_id/run_name/session_id) — injected, never ambient (docs/ir.md).
local function new_run_context(meta)
  run_seq = run_seq + 1
  local scope = "r" .. tostring(run_seq)
  local log = make_logger(scope)

  local ctx = {
    scope = scope,
    run_id = meta.run_id,
    run_name = meta.run_name,
    session_id = meta.session_id,
    last_output_path = nil,
    run_complete = nil,
    run_failed = nil,
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
    if event.kind == observer.EVENTS.run_complete then
      -- Attach the path the sink's writer persisted this run (recorded by
      -- persist_output below) and stash the signal — result included — for
      -- take_run_complete. The sink's own `persisted` flag only says a writer
      -- was wired; the kernel knows whether a write actually landed (the
      -- persistence lib may be absent or the write may have failed), so the
      -- flag surfaced to the control plane is recomputed from that truth.
      event.output_path = ctx.last_output_path
      event.persisted = ctx.last_output_path ~= nil
      ctx.run_complete = {
        output_path = ctx.last_output_path,
        persisted = event.persisted,
        result = event.result,
      }
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
    end
  end

  -- Injected host bus seam. Routing hands this tool.invoke-shaped envelopes
  -- ({ kind = "tool.invoke", id, name, args }) and raw signal-time envelopes
  -- (a dying llm's `<provider>.chat.cancel`). Before an envelope reaches the
  -- shared bus, any provider chat handle it carries — `chat_id` top-level
  -- (cancel envelopes) or under `args` (provider-class invokes,
  -- factories/llm.lua build_request) — is prefixed with this run's scope, so
  -- two runs of the same program hold disjoint provider-side chats and the
  -- bridge's chat-keyed correlation never crosses runs. The rewrite copies
  -- rather than mutates: the emitting actor's own tables stay untouched.
  local function scope_chat_id(chat_id)
    return scope .. "/" .. chat_id
  end
  local function bus_emit(envelope)
    if type(envelope) ~= "table" then
      return
    end
    local out = envelope
    if type(envelope.chat_id) == "string" or
        (type(envelope.args) == "table" and type(envelope.args.chat_id) == "string") then
      out = {}
      for k, v in pairs(envelope) do
        out[k] = v
      end
      if type(out.chat_id) == "string" then
        out.chat_id = scope_chat_id(out.chat_id)
      end
      if type(out.args) == "table" and type(out.args.chat_id) == "string" then
        local args = {}
        for k, v in pairs(out.args) do
          args[k] = v
        end
        args.chat_id = scope_chat_id(args.chat_id)
        out.args = args
      end
    end
    nefor.emit(out)
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
  local router = routing.new({
    inventory = inv,
    registry = registry,
    log = log,
    bus_emit = bus_emit,
    events = emit_event,
    persist_output = persist_output,
    -- Host clock for the busy-window stamps (mag.actor_idle's busy_ms).
    -- nil-safe: routing falls back to a zero clock where the host surface
    -- lacks now_ms (the bare-VM test stub).
    now_ms = nefor.now_ms,
    gen_id = function()
      cap_seq = cap_seq + 1
      return scope .. "/cap-" .. tostring(cap_seq)
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
    local deps = {
      writer = function(output)
        return persist_output(record.id, output)
      end,
    }
    return registry:construct(record.factory, record.id, record.params, emit, deps)
  end)
  inv.set_deliver(function(to, from, content)
    content = content or {}
    router:deliver(to, from, content.kind, content)
  end)

  -- Observability: the observer wraps apply, deriving lifecycle events and one
  -- ordered modification-log entry from the fold boundary (observer.lua). The
  -- inventory itself stays pure; this is the composition layer.
  local mlog = modlog.new({ persist = persist_modlog_entry })
  local obs = observer.new({ inventory = inv, emit_event = emit_event, modlog = mlog })

  ctx.inventory = inv
  ctx.router = router
  ctx.modlog = mlog
  ctx.observer = obs
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

nefor.log("mag-kernel ready (factories: stub, sink, human, llm, run-tool, tool-result, adapter, bash)")

return {
  name = "mag-kernel",

  -- Begin a run: create its context, then emit mag.run_started before the
  -- first modification. Run identity is injected, never ambient. Returns { ok = true, reaped = {...} } or
  -- { ok = false, error } — a duplicate live run_id rejects (the id is the
  -- context key and the reply correlation; two runs may not share it).
  --
  -- Session-boundary reaping: the engine (and this resident kernel) outlives
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
    ctx.observer:run_started({ run_id = ctx.run_id, run_name = ctx.run_name, scope = ctx.scope })
    return { ok = true, reaped = stale }
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
  start = function(run_id, mod)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    local boundary = mod and mod.result and mod.result.from
    if type(boundary) ~= "table" or type(boundary.actor) ~= "string"
        or type(boundary.wire) ~= "string" then
      return { ok = false, error = "initial artifact needs result.from { actor, wire }" }
    end
    local source
    for _, spec in ipairs(mod.actors or {}) do
      if spec.id == boundary.actor then
        source = spec
        break
      end
    end
    if not source then
      return { ok = false, error = string.format(
        "result boundary source actor %q does not exist", boundary.actor) }
    end
    local declaration = registry:declaration(source.factory)
    local declared = false
    for _, output in ipairs((declaration and declaration.outputs) or {}) do
      if output == boundary.wire then
        declared = true
        break
      end
    end
    if not declared then
      return { ok = false, error = string.format(
        "result boundary wire %q is not a declared output of actor %q",
        boundary.wire, boundary.actor) }
    end
    ctx.router:set_result_boundary({ actor = boundary.actor, wire = boundary.wire })
    local modification = {}
    for key, value in pairs(mod) do
      if key ~= "result" then
        modification[key] = value
      end
    end
    return ctx.observer:apply(modification)
  end,

  -- Apply one graph modification through a run's fold. Strictly serialized
  -- within the run — one call, one modification (docs/ir.md).
  apply = function(run_id, mod)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    return ctx.observer:apply(mod)
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
    return ctx.router:drain(id)
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
  --
  -- TERMINATING (`terminate` truthy — a dispatched sub-run): a dispatched run
  -- is ephemeral (its only output is the relayed result), so an interrupt must
  -- STOP it. Cancel the in-flight work (a `tool.cancel` per open correlation —
  -- bash dies via killpg, a nested sub-run is interrupted down the chain) but
  -- deliver NO reply, so the run's llm never re-fires to a "Completed" answer.
  -- The host then ends the run FAILED. Returns { ok = true, interrupted =
  -- <count>, terminated = true }. Unknown run rejects; nothing in flight → 0.
  interrupt_run = function(run_id, failure, terminate)
    local ctx, err = context_of(run_id)
    if not ctx then
      return { ok = false, error = err }
    end
    if terminate then
      local cancelled = ctx.router:cancel_inflight()
      -- Observable marker; the host follows with end_run(failed) and a
      -- `mag.run_result status:"failed"` — no re-fire, the run is over.
      ctx.emit_event({
        kind = "mag.run_interrupted",
        interrupted = cancelled,
        terminated = true,
      })
      return { ok = true, interrupted = cancelled, terminated = true }
    end
    local settled = ctx.router:interrupt(failure)
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

  -- Plain-data foreign contracts supplied to MAG compilation as immutable
  -- input. Qualified identity is the authored/lowered name; implementation is
  -- retained only so the runtime can bind it to the resident constructor.
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
    local rc = ctx.run_complete
    ctx.run_complete = nil
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

  -- Deliver a correlated capability response (tool.result-shaped:
  -- { id, result | error }) back to the requesting actor. Correlation ids are
  -- scope-prefixed and kernel-unique, so trying each live run finds at most
  -- one owner; the owning run's id is returned (nil when the id is not ours —
  -- another consumer's reply on the broadcast bus). The host uses the return
  -- to settle exactly the run the response advanced.
  bus_response = function(response)
    for run_id, ctx in pairs(runs) do
      if ctx.router:bus_response(response) then
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
