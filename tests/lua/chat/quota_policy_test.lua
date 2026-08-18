local quota_policy = require("libs.chat.quota_policy")

local function eq(actual, expected, message)
  assert(actual == expected, message .. ": expected " .. tostring(expected)
    .. ", got " .. tostring(actual))
end

local commands = {
  { name = "model" },
  { name = "usage" },
  { name = "help" },
}

assert(quota_policy.surfaces_enabled(nil), "missing policy preserves quota surfaces")
assert(quota_policy.surfaces_enabled({}), "empty policy preserves quota surfaces")
eq(#quota_policy.filter_commands(commands, {}), 3,
  "generic providers keep configured quota commands")

local no_quota = { quota = "none" }
assert(not quota_policy.surfaces_enabled(no_quota),
  "explicit no-quota policy disables quota surfaces")
local visible = quota_policy.filter_commands(commands, no_quota)
eq(#visible, 2, "only the quota command is hidden")
eq(visible[1].name, "model", "command order is preserved before usage")
eq(visible[2].name, "help", "command order is preserved after usage")
eq(#commands, 3, "filtering does not mutate the canonical command list")

local ok, message = pcall(quota_policy.surfaces_enabled, { quota = "unlimited" })
assert(not ok and tostring(message):find("usage.quota must be `none`", 1, true),
  "unknown policy values fail rather than being guessed")

print("quota_policy_test: all assertions passed")
