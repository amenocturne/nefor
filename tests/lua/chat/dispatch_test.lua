local dispatch = require("libs.chat.dispatch")

local function assert_eq(actual, expected, label)
  assert(actual == expected, label .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
end

local first = function() return "first" end
local second = function() return "second" end
local base = dispatch.group("base", { alpha = first })
local extra = dispatch.group("extra", { beta = second })
local combined = dispatch.combine({ base, extra })
assert_eq(combined.alpha, first, "combines first group")
assert_eq(combined.beta, second, "combines second group")
assert_eq(combined.unknown, nil, "unknown registration stays absent")

local ok, message = pcall(dispatch.combine, {
  base,
  dispatch.group("replacement", { alpha = second }),
})
assert(not ok, "duplicate handlers fail by default")
assert(tostring(message):find("base and replacement", 1, true), "duplicate error names both groups")

local replaced = dispatch.combine({ base, dispatch.group("replacement", { alpha = second }) }, {
  duplicate = "replace",
})
assert_eq(replaced.alpha, second, "replace policy deterministically keeps later handler")

local kept = dispatch.combine({ base, dispatch.group("replacement", { alpha = second }) }, {
  duplicate = "keep",
})
assert_eq(kept.alpha, first, "keep policy deterministically keeps earlier handler")

local bad_policy_ok = pcall(dispatch.combine, {}, { duplicate = "mystery" })
assert(not bad_policy_ok, "unknown duplicate policy fails")

print("dispatch_test: all assertions passed")
