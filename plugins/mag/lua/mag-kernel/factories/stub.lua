-- plugins/mag/lua/mag-kernel/factories/stub.lua — a minimal factory with a declared
-- contract, used by the contract/registry tests.
--
-- This file is the whole truth about the stub actor (actor-model.md, Signals:
-- "reading a factory definition shows exactly which signals it handles").
-- Nothing wraps it; the `drain` handler below is written out explicitly, and
-- the declaration's `signals` list matches what the constructor implements.

local kinds = require("kinds")

local M = {}

-- Plain, readable declaration data. The kernel and the validator read this;
-- no behavior is derived from it.
M.declaration = {
  name = "stub",

  -- params schema — documentation-grade shape of the setup table the
  -- constructor accepts. Opaque to the kernel (params belong to the factory).
  params = {
    greeting = "string?", -- optional label echoed on output
    value = "data?", -- optional canonical payload for rule-kernel tests
    canonical_from_message = "bool?",
  },

  -- Input ports, each a firing-bearing shape (shape.lua). The stub declares
  -- one single-typed input: it fires per arriving message.
  inputs = {
    input = "stub.In",
    forward = "stub.Out",
  },

  -- Output tags this factory can produce (fully-qualified). Composition
  -- checks route keys against exactly this list.
  outputs = {
    "stub.Out",
  },

  -- Signals the factory handles. Declared here AND implemented in construct;
  -- the two must agree by reading, not by machinery.
  signals = {
    "drain",
  },
}

-- construct(id, params, emit, deps) -> instance
--
-- `emit` is the kernel's outbound sink (the actor's whole world). `deps` carries
-- kernel-injected capabilities (unused by the stub — it holds no external
-- seam). The instance signs every message with `id` and confirms creation with
-- a ready message for that id — construction happens at the first activation,
-- so the confirm coincides with beginning work (actor-model.md, Lifecycle).
function M.construct(id, params, emit, deps)
  params = params or {}

  -- Sign: stamp the actor id onto every outbound message so each instance is
  -- individually addressable (actor-model.md, Factories: "sign with the id").
  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). The stub is single-input and synchronous: it reads the one
  -- delivered message, emits its declared `stub.Out` (id-signed, routed by
  -- tag), and returns a successful completion so the kernel emits mag.Unit
  -- along this actor's dependency edges.
  function instance.deliver(activation)
    activation = activation or {}
    local one = (activation.messages or {})[1] or {}
    local value = params.value
    if params.canonical_from_message then value = one.message.value end
    emit(sign({
      kind = "stub.Out",
      greeting = params.greeting,
      value = value,
      payload = one.message,
    }))
    return { status = "ok" }
  end

  -- Explicit drain handler (SIGTERM analog): flush, then a signed completion.
  -- Written inline — no wrapper composed this in. The completion ack is the
  -- reserved kinds.complete (the kernel routes mag.Unit along dependency edges);
  -- there is no separate "Completed" kind.
  function instance.handle_drain()
    emit(sign({ kind = kinds.complete }))
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
