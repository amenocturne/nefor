-- starter/combinators.lua — engine-side composer for the
-- nefor-combinators plugin. The binary speaks the canonical wire shape
-- directly (no translation primitives needed), so the actor spec is
-- built inline via `actor.identity_spec` — the generic identity-
-- passthrough helper in core.actor.

local actor = require("core.actor")
local config = require("config")

return actor.identity_spec("nefor-combinators", {
  config.bin("nefor-combinators"),
})
