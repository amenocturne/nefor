-- plugins/mag/lua/mag-kernel/factories/sink.lua — the program's terminal marker.
--
-- One sink per program (docs/lowering.md, "The program sink"). Terminality is
-- structural, not a flag: its only declared output is the kernel completion
-- wire and it lowers with `routes: {}`, so nothing routes downstream. On activation it does the
-- two jobs a terminal owes the control plane:
--
--   1. Select the persisted final output. The sink strips transcript metadata
--      as the final result without carrying conversation reconstruction data.
--      The kernel's terminal settlement boundary performs the actual write, so
--      persistence and completion share one first-write-wins decision.
--
--   2. Propose run completion to the control plane via `emit`, using the
--      reserved kind `mag.RunComplete` (flagged for review). A signal is just
--      a message with a reserved kind (actor-model.md, Signals); this one
--      carries the final result and whether it was persisted, so the control
--      plane can read the run result and mark the run done.
--
-- Input contract: `( generic-provider.FinalAnswer | mag.Text | human.Approved )`
-- (union) — the fixture's `code-writer.llm -> sink` edge, the shell chain's
-- implicit terminal edge (`bash -> sink`, docs/lowering.md "The program
-- sink"), and the gate template's approval exit (an approval-terminated
-- program ends in `human.Approved` — examples/nefor-agent/mag/lib/templates.mag). No
-- signal handlers: the sink persists synchronously on activation, holding no
-- pending work to drain or abort.

local kinds = require("kinds")

local M = {}

M.declaration = {
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

  -- No authored params; terminal persistence belongs to the kernel boundary.
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

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). Single input: fires per arriving FinalAnswer. Synchronous —
  -- propose run completion and return a successful activation completion.
  function instance.deliver(activation)
    activation = activation or {}
    local final = ((activation.messages or {})[1] or {}).message
    local to_persist = final
    if type(deps.writer) == "function" and not deps.persistence_owned_by_kernel then
      deps.writer(to_persist)
    end
    -- Run-complete signal: the reserved terminal MESSAGE kind for the control
    -- plane (kinds.RunComplete). Routing surfaces it as the mag.run_complete
    -- lifecycle event — two names, two channels (kinds.lua).
    emit(sign({
      kind = kinds.RunComplete,
      result = final,
      persist_result = to_persist,
      persisted = type(deps.writer) == "function",
    }))
    return { status = "ok" }
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens at
  -- the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
