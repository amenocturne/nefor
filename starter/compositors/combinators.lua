-- starter/compositors/combinators.lua — engine-side composer for the
-- nefor-combinators plugin. The binary speaks the canonical wire shape
-- directly (no translation needed), so the actor spec is built inline
-- via `core.actor.identity_spec`.

local actor = require("core.actor")
local config = require("config")

return actor.identity_spec("nefor-combinators", {
  config.bin("nefor-combinators"),
})
