-- plugins/mag/lua/mag-kernel/shape.lua — algebraic type shapes for factory contracts.
--
-- A factory declares the input shapes it accepts and the output tags it
-- produces (see plugins/mag/docs/actor-model.md, Factories; docs/ir.md,
-- Firing). Type tags are fully-qualified name strings exactly as the
-- compiler's `qualify_type` emits them (`generic-provider.ProviderOut`,
-- `mag.Task`). Composition type-checks against these declared
-- shapes; nothing sniffs a runtime value's shape.
--
-- The input shape is also a firing fact (docs/ir.md, Firing):
--   single  `A`          fires per message   — every arriving A activates
--   union   `(A | B)`    fires on any        — whichever arrives activates
--   product `(A + B)`    fires on all        — kernel assembles one activation
--
-- Data representation (plain, readable data — no metatables, no behavior):
--   single  -> a string tag                       "generic-provider.ProviderOut"
--   union   -> a list of tags                      { "A", "B" }
--   product -> a struct keyed by `product`         { product = { "A", "B" } }
--
-- single is the string, union is the bare list of tags, product is the one
-- distinct structure. The three are unambiguous: a string is single, a table
-- with a `product` field is product, any other (array) table is union.

local shape = {}

-- ---- constructors (sugar; the raw data forms above are equally valid) -------

function shape.single(tag)
  assert(type(tag) == "string", "single shape tag must be a string")
  return tag
end

function shape.union(tags)
  assert(type(tags) == "table", "union shape must be a list of tags")
  assert(#tags >= 1, "union shape needs at least one tag")
  local out = {}
  for i, t in ipairs(tags) do
    assert(type(t) == "string", "union tag must be a string")
    out[i] = t
  end
  return out
end

function shape.product(tags)
  assert(type(tags) == "table", "product shape must be a list of tags")
  assert(#tags >= 1, "product shape needs at least one component")
  local out = {}
  for i, t in ipairs(tags) do
    assert(type(t) == "string", "product component must be a string")
    out[i] = t
  end
  return { product = out }
end

-- ---- classification ---------------------------------------------------------

-- Return "single" | "union" | "product" for a shape, or nil + error message
-- for anything that is not a well-formed shape.
function shape.classify(s)
  if type(s) == "string" then
    return "single"
  end
  if type(s) == "table" then
    if s.product ~= nil then
      if type(s.product) ~= "table" or #s.product < 1 then
        return nil, "product shape must carry a non-empty component list"
      end
      return "product"
    end
    if #s >= 1 then
      return "union"
    end
    return nil, "empty table is neither a union nor a product shape"
  end
  return nil, "shape must be a string (single), a list (union), or { product = {...} }"
end

-- The firing semantics a shape carries when used as an input contract
-- (docs/ir.md, Firing). Output shapes reuse the same representation but
-- firing is only meaningful on the input side.
local FIRING = {
  single  = "per-message", -- every arriving message is one activation
  union   = "any",         -- whichever variant arrives first activates alone
  product = "all",         -- accumulate every component, then one activation
}

function shape.firing(s)
  local kind, err = shape.classify(s)
  if not kind then
    return nil, err
  end
  return FIRING[kind]
end

-- The flat list of tags a shape mentions. For single that is one tag; for a
-- union its variants; for a product its components.
function shape.tags(s)
  local kind, err = shape.classify(s)
  if not kind then
    error(err, 2)
  end
  if kind == "single" then
    return { s }
  end
  if kind == "product" then
    local out = {}
    for i, t in ipairs(s.product) do
      out[i] = t
    end
    return out
  end
  local out = {}
  for i, t in ipairs(s) do
    out[i] = t
  end
  return out
end

-- ---- wiring compatibility ---------------------------------------------------

-- Does an input shape accept a single output tag arriving on one edge?
--
-- One route edge carries exactly one fully-qualified output tag. The
-- destination's declared input shape decides whether that tag is a valid
-- input, independent of firing:
--   single  A     -> accepts iff tag == A
--   union   A|B   -> accepts iff tag is one of the variants
--   product A+B   -> accepts iff tag is one of the components (that edge fills
--                    the matching slot; firing waits for all slots elsewhere)
-- Slot accounting for products (which slot a tag fills, one-activation-per-set)
-- is the kernel's fold responsibility, not the contract check.
function shape.accepts(input_shape, tag)
  local kind, err = shape.classify(input_shape)
  if not kind then
    error(err, 2)
  end
  for _, t in ipairs(shape.tags(input_shape)) do
    if t == tag then
      return true
    end
  end
  return false
end

return shape
