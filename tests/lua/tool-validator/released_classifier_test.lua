-- The starter's classifier flags against a released `da` binary
-- (REAL_CLASSIFIER). Opt-in; see engine/tests/starter_tool_validator_test.rs.

local validator = require("tool-validator").build { classifier = REAL_CLASSIFIER }

local function expect(policy, command, expected)
  local result, reason = validator.classify_command(policy, command)
  print(string.format("%-8s %-28s -> %s%s", policy, command, result, reason and (" (" .. reason .. ")") or ""))
  if expected ~= nil and result ~= expected then
    error(policy .. " `" .. command .. "`: expected " .. expected .. ", got " .. tostring(result), 2)
  end
  return result
end

-- A read is safe under both policies.
expect("ReadOnly", "ls -la", "safe")
expect("Default", "ls -la", "safe")
-- A write is not shown to be read-only: no rule recognises it and the
-- ReadOnly stack does not consult the model.
expect("ReadOnly", "touch x", "unsure")
-- Under Default the model answers; whatever it says is reported.
expect("Default", "touch x", nil)
-- The model's "dangerous" class is a denial.
expect("Default", "rm -rf /", "denied")
expect("ReadOnly", "rm -rf /", "unsure")
-- An argument vector is classified as the quoted command.
assert(validator.classify_argv("ReadOnly", { "cat", "README.md" }) == "safe")
assert(validator.classify_argv("ReadOnly", { "rm", "-rf", "build" }) == "unsure")
-- A usage error from the classifier is an error, not a decision.
local broken = require("libs.tool-validator").build {
  classifier = REAL_CLASSIFIER,
  classifier_args = { ReadOnly = { "--no-such-flag" }, Default = { "--no-such-flag" } },
  command_fastpaths = {},
}
local ok, err = pcall(broken.classify_command, "Default", "ls")
assert(not ok and tostring(err):find("unknown status 64", 1, true), tostring(err))
print("released_classifier_test: all assertions passed")
