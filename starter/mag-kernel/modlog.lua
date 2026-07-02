-- starter/mag-kernel/modlog.lua — the modification log ("the modification log
-- is the run"; docs/ir.md, Application semantics).
--
-- An ordered, append-only record of every modification the fold saw, tagged
-- with its outcome — applied / rejected / no-op — and carrying the modification
-- itself. Graph state at any moment is a prefix-fold of this log, so replay is
-- deterministic from the log alone even though arrival order was a race.
--
-- Pure structure: it holds the in-memory list and calls an INJECTED `persist`
-- function once per entry (one JSONL line fits the repo's session conventions;
-- docs/ir.md). Persistence is the wiring's concern — the log stays unit-testable
-- in a bare Lua VM with a capturing persist stub. The observer
-- (starter/mag-kernel/observer.lua) is what classifies outcomes and records
-- here; this module never reaches into the inventory itself.

local M = {}
M.__index = M

-- The three outcomes an apply can have (docs/ir.md): a validated modification
-- that changed state (`applied`), a validation failure (`rejected`), or a
-- validated modification whose every operation was a race-artifact no-op
-- (`noop`). Rejected leaves the inventory untouched; noop is an identity fold.
M.OUTCOME = {
  applied = "applied",
  rejected = "rejected",
  noop = "noop",
}

local function noop() end

-- Construct a modification log. `opts.persist` is `fn(entry) -> ()` — the
-- injected durability seam (one JSONL line per entry); defaults to a no-op so
-- the in-memory log always works on its own.
function M.new(opts)
  opts = opts or {}
  return setmetatable({
    log = {},
    persist = opts.persist or noop,
    seq = 0,
  }, M)
end

-- Append one entry in order. Stamps a monotone `seq`, keeps it in memory, and
-- flushes it through the injected persist sink. Returns the stored entry.
function M:record(entry)
  self.seq = self.seq + 1
  entry.seq = self.seq
  self.log[#self.log + 1] = entry
  self.persist(entry)
  return entry
end

-- The ordered entry list (read-only to callers).
function M:all()
  return self.log
end

-- Entry count.
function M:count()
  return #self.log
end

-- Replay the log back into a fresh inventory (deterministic reconstruction from
-- the log alone; docs/ir.md, "The modification log is the run"). Re-applies each
-- non-rejected entry's modification in order — a `noop` re-applies as a no-op
-- and an `applied` re-applies its state change, so the result matches the live
-- inventory. Rejected entries left the live inventory untouched (identity), so
-- replaying them would be a no-op and they are skipped. `inv` is a caller-
-- supplied fresh inventory (kept an argument so this module never depends on how
-- an inventory is constructed). Returns `inv`.
function M.replay(entries, inv)
  for _, entry in ipairs(entries or {}) do
    if entry.outcome ~= M.OUTCOME.rejected then
      inv.apply(entry.modification)
    end
  end
  return inv
end

return M
