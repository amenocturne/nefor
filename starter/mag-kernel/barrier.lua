-- starter/mag-kernel/barrier.lua — the program-start ready barrier.
--
-- Running a program is a two-step handshake, not fire-and-forget (docs/ir.md,
-- "Running a program"; actor-model.md, Lifecycle). Applying a program's
-- initial modification must:
--
--   1. Spawn every actor in the initial constellation (register + construct).
--   2. Await each actor's ready confirmation (the factory's `mag.ready` emit,
--      intercepted by routing → routing:is_ready).
--   3. Deliver the initial messages only once the whole constellation is
--      ready — strictly after every actor has confirmed.
--
-- This module composes the fold (inventory.apply: register + queue + construct)
-- with the router's barrier hold. It owns no state of its own beyond a plain
-- handle; the readiness state lives in routing, the lifecycle in the inventory.
--
-- ── time is injected ─────────────────────────────────────────────────────────
-- The kernel Lua has no event loop, so the barrier never blocks. Instead the
-- host drives it: `start` opens the barrier and polls once; if the whole
-- constellation is already ready (the shipped factories confirm synchronously
-- in their constructor) it releases immediately. Otherwise the host calls
-- `poll(handle, now_ms)` when a late `mag.ready` arrives or on a timer tick.
-- All time enters through the caller-supplied `now` (a `fn() -> ms`) and the
-- `now_ms` passed to poll — no wall-clock is read here.
--
-- ── deadline shape (decided; flagged) ────────────────────────────────────────
-- PER-PROGRAM deadline, one budget for the whole barrier, measured from
-- `start`. Rationale: the initial constellation spawns atomically in one
-- modification, so every actor's clock starts together — a single shared
-- deadline is the natural unit and names the whole straggler set at once. A
-- per-actor deadline would only differ if actors were spawned at staggered
-- times, which the initial modification never does. Default DEFAULT_DEADLINE_MS
-- (below); overridable per call via opts.deadline_ms. On expiry the run fails
-- with an error naming exactly the actors that never confirmed, and the initial
-- messages stay queued (undelivered) — a failed start delivers nothing.

local M = {}

-- Default per-program barrier budget. The shipped factories ready
-- synchronously, so this only bites once a factory does async construction
-- (e.g. a provider that handshakes before confirming). Coarse on purpose.
local DEFAULT_DEADLINE_MS = 5000

-- Sorted list of the keys of a set — deterministic straggler naming.
local function sorted_keys(set)
  local out = {}
  for k in pairs(set) do
    out[#out + 1] = k
  end
  table.sort(out)
  return out
end

-- Recompute the still-awaiting set from routing readiness; returns the
-- remaining set and whether any remain.
local function still_awaiting(handle)
  local remaining = {}
  local any = false
  for id in pairs(handle.awaiting) do
    if not handle.router:is_ready(id) then
      remaining[id] = true
      any = true
    end
  end
  return remaining, any
end

-- Advance the barrier against the current time `t` (ms). Releases and drains
-- the initial messages once the constellation is ready; fails naming the
-- stragglers once the deadline passes. Idempotent after it settles.
function M.poll(handle, t)
  if handle.done then
    return handle
  end
  local remaining, any = still_awaiting(handle)
  handle.awaiting = remaining
  if not any then
    handle.router:release_barrier()
    handle.done = true
    handle.ok = true
    handle.released = true
    return handle
  end
  if t >= handle.deadline then
    handle.stragglers = sorted_keys(remaining)
    handle.done = true
    handle.ok = false
    handle.released = false
    handle.error = "ready barrier: actors did not confirm ready before deadline: "
      .. table.concat(handle.stragglers, ", ")
    -- Deliberately do NOT release: a failed start delivers no initial messages.
    return handle
  end
  return handle
end

-- Open the barrier and apply the initial modification. `opts`:
--   inventory   the fold (register + queue + construct)
--   router      the delivery layer (barrier hold + readiness)
--   mod         the program's initial modification
--   now         fn() -> ms (injected clock); defaults to a zero clock
--   deadline_ms per-program budget (default DEFAULT_DEADLINE_MS)
-- Returns a handle. `handle.done` tells whether it already settled; if not,
-- the host must poll(handle, now_ms) as readies arrive / on timer ticks.
function M.start(opts)
  opts = opts or {}
  local inv = assert(opts.inventory, "barrier needs an inventory")
  local router = assert(opts.router, "barrier needs a router")
  local now = opts.now or function()
    return 0
  end
  local deadline_ms = opts.deadline_ms or DEFAULT_DEADLINE_MS

  router:begin_barrier()
  local res = inv.apply(opts.mod)
  if not res.ok then
    -- Validation rejected the initial modification: nothing spawned, nothing
    -- held. Clear the barrier and surface the rejection.
    router:release_barrier()
    return { done = true, ok = false, error = res.error, phase = "apply" }
  end

  local handle = {
    router = router,
    awaiting = res.spawned or {},
    deadline = now() + deadline_ms,
    done = false,
  }
  -- Poll once: synchronous factories are already ready, so a well-formed
  -- static program releases here without the host ever polling.
  return M.poll(handle, now())
end

return M
