-- starter/compositors/graph.lua — engine-side composer for the
-- reasoner-graph plugin. The binary speaks the canonical wire shape
-- directly (no translation needed), so the actor spec is built inline
-- via `core.actor.identity_spec`.

local actor = require("core.actor")

local M = {}

function M.spawn_spec(command)
  return actor.identity_spec("reasoner-graph", command)
end

return M
