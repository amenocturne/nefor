-- starter/reasoner-graph/init.lua — wrapper actor for the
-- reasoner-graph Rust binary.
--
-- Per the Phase 3a refactor plan: this wrapper is identity. The
-- agentic-loop owns all run_complete handling + replay-mode gating
-- (it short-circuits in `receive_msg` when replay_mode is true). The
-- resident-reasoner actor handles `<token>.run_node` dispatch and
-- registration on `reasoner-graph.ready`.
--
-- Phase 3b will switch reasoner-graph to speak the canonical tool
-- contract (`tool.invoke` / `tool.result`); at that point the
-- wrapper may want from_plugin / to_plugin shims for the transition.
-- For now: just spawn the binary.

local M = {}

function M.spawn_spec(command)
  assert(type(command) == "table", "reasoner-graph.spawn_spec: command required")

  return {
    name        = "reasoner-graph",
    command     = command,
    receive_msg = function(_) end,
  }
end

return M
