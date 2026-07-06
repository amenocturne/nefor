-- Re-export shim. The lead-workflow mechanism moved to the shared lua tree
-- (`libs/lead-workflow/`); this keeps `require("lead-workflow")` resolving for
-- starter/init.lua's spawn site (which is fenced). Follow-up: point that spawn
-- at `require("libs.lead-workflow")` and delete this shim.
return require("libs.lead-workflow")
