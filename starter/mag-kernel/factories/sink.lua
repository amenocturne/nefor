-- starter/mag-kernel/factories/sink.lua — the program's terminal marker.
--
-- One sink per program (docs/lowering.md, "The program sink"). Terminality is
-- structural, not a flag: the sink declares empty `outputs` and lowers with
-- `routes: {}`, so nothing routes downstream of it. On activation it does the
-- two jobs a terminal owes the control plane:
--
--   1. Persist the final output. Output persistence is the kernel's
--      responsibility (docs/ir.md, division-of-responsibility table), not the
--      factory's — so the sink does NOT hard-wire a file path or an io.open.
--      It calls an INJECTED writer function. The seam: `params.writer` is a
--      `fn(final_output)` supplied by the kernel wiring when it materializes
--      the sink (NOT authored in the MAG modification's params — MAG params
--      are plain JSON data; the writer is a kernel-side closure over the run's
--      output location). Absent a writer, persistence is skipped and flagged
--      on the run-complete signal (`persisted = false`), so a mis-wired kernel
--      is observable rather than silent.
--
--   2. Signal run completion to the control plane via `emit`, using the
--      reserved kind `mag.RunComplete` (flagged for review). A signal is just
--      a message with a reserved kind (actor-model.md, Signals); this one
--      carries the final result and whether it was persisted, so the control
--      plane can read the run result and mark the run done.
--
-- Input contract: `generic-provider.FinalAnswer` (single), matching the
-- fixture's `code-writer.llm -> sink` and `*.exhaust -> sink` edges. No signal
-- handlers: the sink persists synchronously on activation, holding no pending
-- work to drain or abort.

local M = {}

M.declaration = {
  name = "sink",

  -- No authored params. `writer` is injected by the kernel wiring at construct
  -- time (see the seam note above); it is not part of the MAG program.
  params = {},

  inputs = {
    final = "generic-provider.FinalAnswer",
  },

  -- Terminal: emits nothing downstream (routes: {} at lowering).
  outputs = {},

  signals = {},
}

-- construct(id, params, emit) -> instance
function M.construct(id, params, emit)
  params = params or {}

  -- Kernel-injected persistence seam. `fn(final_output) -> ()`.
  local writer = params.writer

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- Fire per arriving FinalAnswer (single input contract).
  function instance.activate(message)
    local final = message
    local persisted = false
    if type(writer) == "function" then
      writer(final)
      persisted = true
    end

    -- Run-complete signal (flagged): reserved kind for the control plane.
    emit(sign({
      kind = "mag.RunComplete",
      result = final,
      persisted = persisted,
    }))
  end

  -- Ready barrier (actor-model.md, Lifecycle): confirm creation for this id.
  emit(sign({ kind = "mag.ready" }))

  return instance
end

return M
