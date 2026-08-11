-- examples/nefor-agent/lib/ids.lua — id-shape helpers shared across the orchestrator.
--
-- The id_seq counter mint_chat_run_id() folds in for collision
-- resistance lives in envelope.lua so uuid_lite() and
-- mint_chat_run_id() share the same monotonic sequence.

local envelope = require("core.envelope")

local M = {}

function M.mint_chat_run_id()
  return string.format(
    "chat-run-%d-%d-%d",
    os.time(),
    envelope.next_seq(),
    math.random(0, 2 ^ 31 - 1)
  )
end

return M
