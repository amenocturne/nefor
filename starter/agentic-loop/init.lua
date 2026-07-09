-- starter/agentic-loop/init.lua — re-export shim.
--
-- The turn spawner moved to the shared Lua tree at `lua/libs/agentic-loop/`
-- (require as `libs.agentic-loop`); only the config-owned turn-program data
-- (`lead-turn.mag`) stays here. This shim keeps `require("agentic-loop")`
-- resolving for consumers that still reference it by that name.
return require("libs.agentic-loop")
