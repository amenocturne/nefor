-- starter/lib/ids.lua — id-shape helpers shared across the orchestrator.
--
-- mint_chat_run_id() and pending_key() were extracted from
-- agentic_workflow.lua during the Phase 1 refactor. The id_seq counter
-- they fold in for collision resistance lives in envelope.lua so
-- uuid_lite() and mint_chat_run_id() share the same monotonic sequence.

local envelope = require("lib.envelope")

local M = {}

function M.mint_chat_run_id()
  return string.format(
    "chat-run-%d-%d-%d",
    os.time(),
    envelope.next_seq(),
    math.random(0, 2 ^ 31 - 1)
  )
end

function M.pending_key(run_id, firing_id)
  return tostring(run_id) .. ":" .. tostring(firing_id)
end

return M
