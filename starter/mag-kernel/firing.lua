-- starter/mag-kernel/firing.lua — the input-contract firing machine.
--
-- Firing is a type fact, symmetric to routing (docs/ir.md, Firing): output
-- types decide where results go, input types decide when an actor runs. This
-- module is one pure state machine per declared input port:
--
--   single  `A`        fires per message — every arriving A is one activation
--   union   `(A | B)`  fires on any      — whichever variant arrives activates
--   product `(A + B)`  fires on all      — accumulate, one activation per set
--
-- It knows nothing of the inventory, routing, or the bus — it is fed
-- (sender, tag, message) triples and yields assembled activations. The
-- product case is the interesting one: **slot identity is the incoming edge,
-- not the bare type** (docs/ir.md, Firing). Slots are supplied by the caller
-- as sender-bound edges (derived from routes topology), so `(Unit + Unit)`
-- from two distinct upstreams fills two distinct slots, and two completions
-- of the *same* upstream fill one slot twice (per-slot FIFO) rather than
-- masquerading as a complete set.

local shape = require("shape")

local M = {}
M.__index = M

-- Build a firing machine for one declared input port.
--   input_shape : a shape.* value (single string | union list | { product })
--   slot_edges  : product only — an array of { sender = <id>, type = <tag> }
--                 incoming edges, one per slot, derived from routes topology
--                 by the caller (see routing.lua, derive_slots). Ignored for
--                 single/union, which carry no per-slot accumulation.
function M.build(input_shape, slot_edges)
  local kind = assert(shape.classify(input_shape))
  local self = setmetatable({ kind = kind, tags = {} }, M)
  for _, t in ipairs(shape.tags(input_shape)) do
    self.tags[t] = true
  end

  if kind == "product" then
    -- Ordered slot keys + per-slot FIFO queues. The key is the edge
    -- (sender + type), never the bare type, so two edges carrying the same
    -- type stay distinct slots.
    self.slots = {}
    self.queues = {}
    self.slot_of = {}
    for _, e in ipairs(slot_edges or {}) do
      local key = (e.sender or "") .. "\0" .. e.type
      if not self.queues[key] then
        self.slots[#self.slots + 1] = key
        self.queues[key] = {}
        self.slot_of[key] = { sender = e.sender, type = e.type }
      end
    end
  end

  return self
end

-- Does this port's input contract mention `tag`? (single: the one tag; union:
-- any variant; product: any component.) Independent of firing readiness — it
-- answers "would an arriving `tag` belong to this port at all".
function M:accepts(tag)
  return self.tags[tag] == true
end

-- Offer one arriving message; return the (possibly empty) list of activations
-- it produces. A single/union arrival yields exactly one activation carrying
-- that message. A product arrival is filed into its sender-bound slot and
-- yields one activation per complete set now assembleable (usually zero or
-- one, but a backlog can release several).
function M:offer(from, tag, message)
  if not self.tags[tag] then
    return {}
  end

  if self.kind ~= "product" then
    return { { shape = self.kind, messages = { { from = from, tag = tag, message = message } } } }
  end

  -- product: file into the sender-bound slot's FIFO, then drain every
  -- complete set. A component tag arriving from a sender with no bound slot
  -- is an unexpected upstream — not an accepted arrival for this port.
  local key = (from or "") .. "\0" .. tag
  local q = self.queues[key]
  if not q then
    return {}
  end
  q[#q + 1] = { from = from, tag = tag, message = message }

  local activations = {}
  while self:_complete() do
    local msgs = {}
    for i, k in ipairs(self.slots) do
      msgs[i] = table.remove(self.queues[k], 1)
    end
    activations[#activations + 1] = { shape = "product", messages = msgs }
  end
  return activations
end

-- product only: every slot holds at least one queued message.
function M:_complete()
  if #self.slots == 0 then
    return false
  end
  for _, k in ipairs(self.slots) do
    if #self.queues[k] == 0 then
      return false
    end
  end
  return true
end

return M
