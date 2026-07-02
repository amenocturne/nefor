-- starter/mag-kernel/observer.lua — lifecycle events + modification-log
-- recording, derived from the fold boundary.
--
-- The kernel's fold (inventory.lua) stays pure and io-free. This module is the
-- composition layer that turns each apply into observability: it wraps
-- inventory.apply, diffs the per-id lifecycle states around it (through the
-- inventory's public seams — state_of, apply), emits namespaced lifecycle events
-- through an INJECTED emitter (bus-shaped, exactly like routing's bus_emit), and
-- records one ordered entry into the modification log. The inventory is never
-- modified; every fact here is read from its public surface.
--
-- Two lifecycle events originate in the delivery layer, not the fold, and so are
-- emitted by routing.lua through the same injected emitter: `mag.actor_ready`
-- (the ready barrier) and `mag.run_complete` (the sink's mag.RunComplete). The
-- event kinds below are the canonical set; routing references the same strings.
--
-- ── event-kind set (flagged) ────────────────────────────────────────────────
-- Parity+ with reasoner-graph's broadcast markers (graph.run_started,
-- graph.node_dispatched, graph.run_complete); snake_case, `mag.`-qualified to
-- match that convention. The set is deliberately richer than reasoner-graph so
-- the control-plane view is structurally better, not just non-regressed:
--   mag.run_started            a run/program began (wiring, before the first mod)
--   mag.actor_spawned          the fold registered a new id (never-existed→alive)
--   mag.actor_ready            the factory confirmed the instance ready (routing)
--   mag.actor_killed           the fold killed a live id (alive→dead)
--   mag.modification_applied   a validated mod that changed state
--   mag.modification_rejected  a validation failure (carries the error)
--   mag.modification_noop      a validated mod whose ops were all race no-ops
--   mag.run_complete           the sink signaled terminal completion (routing)

local kinds = require("kinds")

local M = {}
M.__index = M

M.EVENTS = {
  run_started = "mag.run_started",
  actor_spawned = "mag.actor_spawned",
  actor_ready = "mag.actor_ready",
  actor_killed = "mag.actor_killed",
  modification_applied = "mag.modification_applied",
  modification_rejected = "mag.modification_rejected",
  modification_noop = "mag.modification_noop",
  -- Shared with routing.lua through kinds.lua: the run-complete event kind
  -- drifted against the RunComplete message kind, so both are sourced here.
  run_complete = kinds.run_complete,
}

local EVENTS = M.EVENTS

local function noop() end

-- Construct an observer. `opts.inventory` is required (the fold it observes).
-- `opts.emit_event` is `fn(event) -> ()`, the injected lifecycle-event sink
-- (defaults to no-op). `opts.modlog` is an optional modification log to record
-- into (modlog.lua).
function M.new(opts)
  opts = opts or {}
  return setmetatable({
    inventory = assert(opts.inventory, "observer needs an inventory"),
    emit_event = opts.emit_event or noop,
    modlog = opts.modlog,
  }, M)
end

-- Emit `mag.run_started`. The wiring/host calls this at the start of a run,
-- before the initial modification is applied, so observers record the run up
-- front (parity with reasoner-graph's RunStarted). Run identity is host-
-- provided and passed through untouched.
function M:run_started(meta)
  meta = meta or {}
  self.emit_event({
    kind = EVENTS.run_started,
    run_id = meta.run_id,
    run_name = meta.run_name,
  })
end

-- Apply one modification through the fold, deriving lifecycle events and one
-- modification-log entry from the state diff around inventory.apply. Returns the
-- inventory's own result verbatim ({ ok = true } | { ok = false, error = ... }),
-- so this is a transparent wrapper for callers.
function M:apply(modification)
  modification = modification or {}
  local pre = self:snapshot(modification)
  local result = self.inventory.apply(modification)
  return self:observe(modification, pre, result)
end

-- Snapshot the pre-apply lifecycle state of every id the modification touches,
-- so the post-apply diff can tell a real transition from a race no-op.
function M:snapshot(modification)
  local pre = {}
  for _, actor in ipairs(modification.actors or {}) do
    if type(actor.id) == "string" then
      pre[actor.id] = self.inventory.state_of(actor.id)
    end
  end
  for _, id in ipairs(modification.kills or {}) do
    if type(id) == "string" then
      pre[id] = self.inventory.state_of(id)
    end
  end
  return pre
end

-- Diff the fold boundary and emit events + record the log entry.
function M:observe(modification, pre, result)
  if not result.ok then
    self.emit_event({
      kind = EVENTS.modification_rejected,
      modification = modification,
      error = result.error,
    })
    self:record(modification, "rejected", { error = result.error })
    return result
  end

  local spawned, killed, noops = {}, {}, {}

  -- Spawns: a real spawn is never-existed → alive; anything else is a monotone
  -- no-op whose flavor mirrors the inventory's own log levels (docs/ir.md).
  for _, actor in ipairs(modification.actors or {}) do
    local id = actor.id
    if type(id) == "string" then
      if pre[id] == "never-existed" and self.inventory.state_of(id) == "alive" then
        spawned[#spawned + 1] = id
        self.emit_event({ kind = EVENTS.actor_spawned, id = id, factory = actor.factory })
      else
        noops[#noops + 1] = {
          op = "spawn",
          id = id,
          flavor = pre[id] == "alive" and "duplicate-alive" or "spawn-on-dead",
        }
      end
    end
  end

  -- Kills: a real kill is alive → dead; a kill on dead/never-existed is a no-op.
  for _, id in ipairs(modification.kills or {}) do
    if type(id) == "string" then
      if pre[id] == "alive" and self.inventory.state_of(id) == "dead" then
        killed[#killed + 1] = id
        self.emit_event({ kind = EVENTS.actor_killed, id = id })
      else
        noops[#noops + 1] = {
          op = "kill",
          id = id,
          flavor = pre[id] == "dead" and "already-dead" or "never-existed",
        }
      end
    end
  end

  -- Messages: a send to a live target (incl. one created in this same mod) is a
  -- real change (it queues); a send to a dead target drops as a no-op.
  local delivered = false
  for _, msg in ipairs(modification.messages or {}) do
    if type(msg.to) == "string" then
      if self.inventory.state_of(msg.to) == "alive" then
        delivered = true
      else
        noops[#noops + 1] = { op = "send", to = msg.to, flavor = "dropped-dead" }
      end
    end
  end

  if #spawned > 0 or #killed > 0 or delivered then
    self.emit_event({
      kind = EVENTS.modification_applied,
      modification = modification,
      spawned = spawned,
      killed = killed,
    })
    self:record(modification, "applied", { spawned = spawned, killed = killed, noops = noops })
  else
    self.emit_event({
      kind = EVENTS.modification_noop,
      modification = modification,
      noops = noops,
    })
    self:record(modification, "noop", { noops = noops })
  end
  return result
end

-- Record one ordered modification-log entry (no-op if no log is wired).
function M:record(modification, outcome, extra)
  if not self.modlog then
    return
  end
  local entry = { outcome = outcome, modification = modification }
  for k, v in pairs(extra or {}) do
    entry[k] = v
  end
  self.modlog:record(entry)
end

return M
