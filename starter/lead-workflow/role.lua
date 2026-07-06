-- Re-export shim. See init.lua in this dir. Keeps `require("lead-workflow.role")`
-- resolving for starter/init.lua. Follow-up: point that at
-- `require("libs.lead-workflow.role")` and delete this shim.
return require("libs.lead-workflow.role")
