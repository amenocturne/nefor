-- plugins/mag/lua/mag-kernel/factories/sink.lua — the program's terminal marker.
--
-- One sink per program (docs/lowering.md, "The program sink"). Terminality is
-- structural, not a flag: its only declared output is the kernel completion
-- wire and it lowers with `routes: {}`, so nothing routes downstream. On activation it does the
-- two jobs a terminal owes the control plane:
--
--   1. Persist the final output. Output persistence is the kernel's
--      responsibility (docs/ir.md, division-of-responsibility table), not the
--      factory's — so the sink does NOT hard-wire a file path or an io.open.
--      It calls an INJECTED writer function. The seam: `deps.writer` is a
--      `fn(final_output)` supplied by the kernel wiring when it materializes
--      the sink (NOT authored in the MAG modification's params — MAG params
--      are plain JSON data; kernel-injected capabilities travel in `deps`, a
--      construct argument distinct from params, and the writer is a kernel-side
--      closure over the run's output location). Absent a writer, persistence is
--      skipped and flagged on the run-complete signal (`persisted = false`), so
--      a mis-wired kernel is observable rather than silent.
--
--   2. Signal run completion to the control plane via `emit`, using the
--      reserved kind `mag.RunComplete` (flagged for review). A signal is just
--      a message with a reserved kind (actor-model.md, Signals); this one
--      carries the final result and whether it was persisted, so the control
--      plane can read the run result and mark the run done.
--
-- Input contract: `( generic-provider.FinalAnswer | mag.Text | human.Approved )`
-- (union) — the fixture's `code-writer.llm -> sink` edge, the shell chain's
-- implicit terminal edge (`bash -> sink`, docs/lowering.md "The program
-- sink"), and the gate template's approval exit (an approval-terminated
-- program ends in `human.Approved` — starter/mag/lib/templates.mag). No
-- signal handlers: the sink persists synchronously on activation, holding no
-- pending work to drain or abort.

local kinds = require("kinds")

local M = {}
local preview_components = require("preview-components")

M.declaration = {
  preview = preview_components.sink(),
  name = "sink",
  type_variables = { "T" },
  semantic = {
    input={kind="variable",name="T"},
    output={kind="primitive",name="Unit"},
    inputs={
      {wire="generic-provider.FinalAnswer",type={kind="variable",name="T"}},
      {wire="mag.Text",type={kind="variable",name="T"}},
      {wire="human.Approved",type={kind="variable",name="T"}},
    },
    outputs={{wire=kinds.Unit,type={kind="primitive",name="Unit"}}},
  },

  -- No authored params. `writer` is injected by the kernel wiring at construct
  -- time via `deps` (see the seam note above); it is not part of the MAG program.
  params = {},

  inputs = {
    -- Union (fires on any): an agent program terminates in a FinalAnswer;
    -- a shell program's implicit terminal receives the last command's stdout
    -- (mag.Text — the bash capability node, factories/bash.lua); a gate
    -- program terminates in the human's approval (human.Approved —
    -- factories/human.lua, the gate template's exit).
    final = { "generic-provider.FinalAnswer", "mag.Text", "human.Approved" },
  },

  -- Completion is kernel-synthesized; the sink itself emits no data output.
  outputs = { kinds.Unit },

  signals = {},
}

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}
  deps = deps or {}

  -- Kernel-injected persistence seam. `fn(final_output) -> ()`. Travels in
  -- `deps`, kept out of the authored `params` (see the seam note above).
  local writer = deps.writer

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Single input: fires per arriving FinalAnswer. Synchronous —
  -- persist via the injected writer, signal run completion, and return a
  -- successful completion.
  function instance.deliver(activation)
    activation = activation or {}
    local final = ((activation.messages or {})[1] or {}).message
    -- The llm's transcript_delta (factories/llm.lua, "Transcript delta") is
    -- the conversation record, not the final output: keep it OFF the persisted
    -- file (sink.output stays the answer alone) but ON the run-complete signal,
    -- where it rides the terminal mag.run_result back to the run's spawner.
    local to_persist = final
    if type(final) == "table" and final.transcript_delta ~= nil then
      to_persist = {}
      for k, v in pairs(final) do
        if k ~= "transcript_delta" then to_persist[k] = v end
      end
    end
    local persisted = false
    if type(writer) == "function" then
      writer(to_persist)
      persisted = true
    end

    -- Run-complete signal: the reserved terminal MESSAGE kind for the control
    -- plane (kinds.RunComplete). Routing surfaces it as the mag.run_complete
    -- lifecycle event — two names, two channels (kinds.lua).
    emit(sign({
      kind = kinds.RunComplete,
      result = final,
      persisted = persisted,
    }))
    return { status = "ok" }
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
