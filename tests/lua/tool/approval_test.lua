-- The approval vocabulary: the policy order and the three decision results.
-- Driven by engine/tests/tool_constructor_test.rs, which sets REPO_ROOT.

local approval = require("libs.tool.approval")

local function eq(actual, expected, label)
  if actual ~= expected then
    error(string.format("%s:\n  expected %s\n  got      %s", label,
      tostring(expected), tostring(actual)), 2)
  end
end

-- The Lua order repeats the MAG type's constructor order.
do
  local handle = assert(io.open(REPO_ROOT .. "/mag/lib/nefor/contracts.mag", "r"))
  local source = handle:read("*a")
  handle:close()
  local declared = assert(source:match("\ntype ToolApprovalPolicy = ([^\n]+)\n"),
    "contracts.mag declares ToolApprovalPolicy on one line")
  local constructors = {}
  for name in declared:gmatch("[%a_][%w_]*") do constructors[#constructors + 1] = name end
  eq(table.concat(constructors, " < "), table.concat(approval.POLICIES, " < "),
    "libs.tool.approval lists the policies in the order MAG declares them")
end

eq(table.concat(approval.POLICIES, " < "), "ReadOnly < Default", "the policy order")
eq(approval.compare("ReadOnly", "Default"), -1, "ReadOnly is narrower than Default")
eq(approval.compare("Default", "ReadOnly"), 1, "Default is wider than ReadOnly")
eq(approval.compare("Default", "Default"), 0, "a policy equals itself")
eq(approval.wider("Default", "ReadOnly"), true, "wider")
eq(approval.wider("ReadOnly", "ReadOnly"), false, "a policy is not wider than itself")
eq(approval.min("Default", "ReadOnly"), "ReadOnly", "min picks the narrower policy")
eq(approval.min("ReadOnly", "Default"), "ReadOnly", "min is symmetric")
eq(approval.rank("ReadOnly") < approval.rank("Default"), true, "rank follows the order")
eq(approval.is_policy("Rules"), false, "only declared policies are policies")
eq(pcall(approval.compare, "Yolo", "Default"), false, "comparing an unknown policy raises")

eq(approval.from_mag({ constructor = "ReadOnly" }), "ReadOnly", "a lowered MAG value names its policy")
local missing, message = approval.from_mag({ rules = {} })
eq(missing, nil, "another shape is not a policy")
eq(message, "expected a ToolApprovalPolicy (ReadOnly | Default)", "the message lists the policies")

-- denied dominates, then unsure, then safe.
for _, case in ipairs({
  { "safe", "safe", "safe" }, { "safe", "unsure", "unsure" }, { "unsure", "safe", "unsure" },
  { "safe", "denied", "denied" }, { "denied", "unsure", "denied" }, { "unsure", "denied", "denied" },
  { "unsure", "unsure", "unsure" }, { "denied", "denied", "denied" },
}) do
  eq(approval.combine(case[1], case[2]), case[3], "combine " .. case[1] .. " with " .. case[2])
end
eq(pcall(approval.combine, "safe", "maybe"), false, "combining an unknown result raises")
eq(approval.is_result("unsure"), true, "unsure is a result")
eq(approval.is_result("approve"), false, "approve is not a result")
