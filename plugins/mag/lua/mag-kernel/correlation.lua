-- plugins/mag/lua/mag-kernel/correlation.lua — capability request/response tracking.
--
-- When an actor asks a capability plugin for something, the request leaves on
-- the bus and the answer comes back on the bus, asynchronously. Something has
-- to remember which actor is owed which answer. This
-- module owns that one concern: a single table from a kernel-minted request id
-- to the requesting actor (plus the actor's own opaque ref), so a `tool.result`
-- routes back to exactly the actor that asked — and nothing else.
--
-- Pure and bus-unaware: it mints ids and remembers owners. Putting the request
-- on the wire and delivering the reply are routing's job (routing.lua).

local M = {}
M.__index = M

-- `gen_id` (optional) mints wire-unique request ids; when absent a private
-- monotone counter is used (deterministic, fine for a single kernel).
function M.new(gen_id)
  return setmetatable({ gen_id = gen_id, pending = {}, seq = 0 }, M)
end

function M:_next_id()
  self.seq = self.seq + 1
  return "cap-" .. self.seq
end

-- Open a correlation for an outbound request; returns the wire request id to
-- stamp on the envelope. `ref` is the actor's own opaque correlation handle,
-- echoed back untouched so a caller with several requests in flight can tell
-- them apart without the kernel understanding its scheme.
function M:open(requester, ref)
  local id = (self.gen_id and self.gen_id()) or self:_next_id()
  self.pending[id] = { requester = requester, ref = ref }
  return id
end

-- Close a request by wire id; returns { requester, ref } or nil (unknown id —
-- not ours, a duplicate reply, or already closed).
function M:close(request_id)
  local entry = self.pending[request_id]
  self.pending[request_id] = nil
  return entry
end

-- The wire ids of every open (unanswered) request, sorted for determinism.
-- The interrupt path snapshots these to settle each in-flight capability as a
-- failure without mutating the table it iterates (routing.lua interrupt).
function M:pending_ids()
  local ids = {}
  for rid in pairs(self.pending) do
    ids[#ids + 1] = rid
  end
  table.sort(ids)
  return ids
end

-- Drop every outstanding request owned by a killed actor. Kill unroutes and
-- drops the mailbox; it drops pending correlations too, so a late reply to a
-- dead requester finds nothing and is discarded.
function M:drop_requester(id)
  for rid, entry in pairs(self.pending) do
    if entry.requester == id then
      self.pending[rid] = nil
    end
  end
end

return M
