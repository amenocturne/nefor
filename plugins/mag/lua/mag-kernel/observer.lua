-- plugins/mag/lua/mag-kernel/observer.lua — lifecycle events + modification-log
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
-- Several lifecycle events originate in the delivery layer, not the fold, and
-- so are emitted by routing.lua through the same injected emitter:
-- `mag.actor_ready` (lazy construction confirmed — the actor began work),
-- `mag.actor_busy` / `mag.actor_idle` (the per-activation busy window),
-- and `mag.run_complete` (the sink's mag.RunComplete). The event kinds below
-- are the canonical set; routing references the same strings.
--
-- ── event-kind set (flagged) ────────────────────────────────────────────────
-- snake_case, `mag.`-qualified broadcast markers:
--   mag.run_started            a run/program began (wiring, before the first mod)
--   mag.actor_spawned          the fold registered a new id (never-existed→alive)
--   mag.actor_ready            the instance constructed at its first activation
--                              and confirmed (routing) — "began work"
--   mag.actor_busy             an activation was delivered to the instance —
--                              the actor's busy window opened (routing)
--   mag.actor_idle             the activation's completion settled — sync
--                              return, async ack, or capability reply; failed
--                              settles included — carrying `busy_ms`, the
--                              window's length (routing). Strictly alternates
--                              with actor_busy per actor
--   mag.actor_killed           the fold killed a live id (alive→dead); carries
--                              `reason` — "modification" for a kill entry in an
--                              applied mod, or the teardown reason the caller
--                              threads through apply's opts ("run_complete" /
--                              "run_failed" / "killed" / "reaped"; init.lua)
--   mag.modification_applied   a validated mod that changed state
--   mag.modification_rejected  a validation failure (carries the error)
--   mag.modification_noop      a validated mod whose ops were all race no-ops
--   mag.run_complete           the sink signaled terminal completion (routing)
--   mag.run_failed             an unhandled actor failure ended the run (routing)

local kinds = require("kinds")

local M = {}
M.__index = M

M.EVENTS = {
  run_started = "mag.run_started",
  actor_spawned = "mag.actor_spawned",
  actor_ready = "mag.actor_ready",
  actor_busy = "mag.actor_busy",
  actor_idle = "mag.actor_idle",
  actor_killed = "mag.actor_killed",
  modification_applied = "mag.modification_applied",
  modification_rejected = "mag.modification_rejected",
  modification_noop = "mag.modification_noop",
  -- Shared with routing.lua through kinds.lua: the run-complete event kind
  -- drifted against the RunComplete message kind, so both are sourced here.
  run_complete = kinds.run_complete,
  -- Shared with routing.lua through kinds.lua: the unhandled-failure
  -- escalation the delivery layer emits (apply_completion).
  run_failed = kinds.run_failed,
}

local EVENTS = M.EVENTS

local function noop() end

-- Construct an observer. `opts.inventory` is required (the fold it observes).
-- `opts.emit_event` is `fn(event) -> ()`, the injected lifecycle-event sink
-- (defaults to no-op). `opts.modlog` is an optional modification log to record
-- into (modlog.lua).
--
-- `mag.actor_spawned` is emitted through the inventory's on_spawn seam, AT
-- REGISTRATION — inside the fold's spawn pass, before any send in the same
-- modification can construct + fire an actor. A post-apply diff would emit it
-- after the ready/output events the sends cascade, inverting the wire order
-- (spawned must precede the ready its first activation triggers).
function M.new(opts)
  opts = opts or {}
  local self = setmetatable({
    inventory = assert(opts.inventory, "observer needs an inventory"),
    emit_event = opts.emit_event or noop,
    modlog = opts.modlog,
  }, M)
  self.inventory.set_on_spawn(function(record)
    self.emit_event({
      kind = EVENTS.actor_spawned,
      id = record.id,
      factory = record.factory,
    })
  end)
  return self
end

-- Emit `mag.run_started`. The wiring/host calls this at the start of a run,
-- before the initial modification is applied, so observers record the run up
-- front. Run identity is host-
-- provided and passed through untouched. `scope` is the run's wire-id scope
-- token (`r<K>` — init.lua, run contexts): everything this run puts on the
-- shared bus that must resolve back to it is `<scope>/`-prefixed (capability
-- correlation ids, provider chat handles), so surfacing the token here lets
-- the run's spawner do prefix-form correlation (e.g. the lead turn's
-- `chat.lead.bound { chat_prefix }`) without ever parsing an opaque id.
function M:run_started(meta)
  meta = meta or {}
  self.emit_event({
    kind = EVENTS.run_started,
    run_id = meta.run_id,
    run_name = meta.run_name,
    session_id = meta.session_id,
    scope = meta.scope,
    principal = meta.principal,
  })
end

-- Apply one modification through the fold, deriving lifecycle events and one
-- modification-log entry from the state diff around inventory.apply. Returns the
-- inventory's own result verbatim ({ ok = true } | { ok = false, error = ... }),
-- so this is a transparent wrapper for callers.
-- `opts.kill_reason` names why this modification's kills happen — display
-- semantics for the control plane, not mechanics. Absent (the normal in-run
-- path) it defaults to "modification"; run-context teardown threads its
-- terminal reason through here (init.lua reap_run).
function M:apply(modification, opts)
  modification = modification or {}
  local pre = self:snapshot(modification)
  local result = self.inventory.apply(modification)
  return self:observe(modification, pre, result, opts)
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
function M:observe(modification, pre, result, opts)
  local kill_reason = (opts and opts.kill_reason) or "modification"
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
  -- The per-actor mag.actor_spawned event already fired at registration (the
  -- on_spawn seam, M.new); this diff only classifies for the applied/noop
  -- summary and the modlog entry.
  for _, actor in ipairs(modification.actors or {}) do
    local id = actor.id
    if type(id) == "string" then
      if pre[id] == "never-existed" and self.inventory.state_of(id) == "alive" then
        spawned[#spawned + 1] = id
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
        self.emit_event({ kind = EVENTS.actor_killed, id = id, reason = kill_reason })
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
