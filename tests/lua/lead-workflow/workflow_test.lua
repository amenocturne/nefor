-- tests/lua/lead-workflow/workflow_test.lua — 0.4 shim smoke test.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

package.preload["libs.lead-workflow"] = function()
  return { name = "lead-workflow", marker = "shared-0.4" }
end

local lw = require("lead-workflow")
assert_true(lw.marker == "shared-0.4", "lead-workflow shim re-exports shared 0.4 lib")
assert_true(lw.gate_against_unapproved_plan == nil,
  "old dispatch-graph preflight helper is not part of the MAG workflow")

print("lead_workflow_test: all assertions passed")
