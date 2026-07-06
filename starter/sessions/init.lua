-- starter/sessions/init.lua — re-export shim.
--
-- The session-management actor moved to the shared Lua tree at
-- `lua/libs/sessions/` (require as `libs.sessions`). This shim keeps
-- `require("sessions")` resolving for consumers that still reference it
-- by that bare name (lead-workflow, the agentic-loop turn spawner, and
-- the Rust test harnesses). The test escape-hatch surface
-- (`require("sessions.test")`) still lives at `tests/lua/sessions/test.lua`.
return require("libs.sessions")
