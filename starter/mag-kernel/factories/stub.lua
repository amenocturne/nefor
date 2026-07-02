-- starter/mag-kernel/factories/stub.lua — a minimal factory with a declared
-- contract, used by the contract/registry tests.
--
-- This file is the whole truth about the stub actor (actor-model.md, Signals:
-- "reading a factory definition shows exactly which signals it handles").
-- Nothing wraps it; the `drain` handler below is written out explicitly, and
-- the declaration's `signals` list matches what the constructor implements.

local M = {}

-- Plain, readable declaration data. The kernel and the validator read this;
-- no behavior is derived from it.
M.declaration = {
  name = "stub",

  -- params schema — documentation-grade shape of the setup table the
  -- constructor accepts. Opaque to the kernel (params belong to the factory).
  params = {
    greeting = "string?", -- optional label echoed on output
  },

  -- Input ports, each a firing-bearing shape (shape.lua). The stub declares
  -- one single-typed input: it fires per arriving message.
  inputs = {
    input = "stub.In",
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

-- construct(id, params, emit) -> instance
--
-- `emit` is the kernel's outbound sink (the actor's whole world). The instance
-- signs every message with `id` and, per the ready barrier, confirms creation
-- immediately with a ready message for that id.
function M.construct(id, params, emit)
  params = params or {}

  -- Sign: stamp the actor id onto every outbound message so each instance is
  -- individually addressable (actor-model.md, Factories: "sign with the id").
  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Emit an id-signed output carrying the declared `stub.Out` tag.
  function instance.emit_output(payload)
    emit(sign({
      kind = "stub.Out",
      greeting = params.greeting,
      payload = payload,
    }))
  end

  -- Explicit drain handler (SIGTERM analog): flush, then a signed completion.
  -- Written inline — no wrapper composed this in.
  function instance.handle_drain()
    emit(sign({ kind = "mag.Completed" }))
  end

  -- Confirm ready for this id (Lifecycle: factory creates instance, emits
  -- ready; the kernel then drains the pending mailbox to it).
  emit(sign({ kind = "mag.ready" }))

  return instance
end

return M
