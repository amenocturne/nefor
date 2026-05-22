-- tests/lua/config_test.lua — variant-table assertions for config.
--
-- The team port pins per-role model identifiers in
-- cfg.workflow.role_models, variant-specific: prod → Nestor cluster,
-- test → ollama, mock → empty by design (mock-plugin dispatches by
-- prompt content, not model name).
--
-- The config module reads NEFOR_CONFIG at module-load to pick
-- M.active. The tests below read M.prod / M.test / M.mock directly so
-- the env-var resolution path is irrelevant.

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local lead_role = require("lead-workflow.role")

local team_roles = {
  "explorer", "builder", "reviewer",
  "tester", "critic", "reflector", "prompt-engineer", "docs",
}

for _, role in ipairs(team_roles) do
  assert_true(lead_role.AGENT_CONFIGS[role] ~= nil,
    "lead-workflow.role AGENT_CONFIGS." .. role .. " must exist")
end

local cfg_module = require("config")

local function role_models_of(variant)
  local v = cfg_module[variant]
  assert_true(type(v) == "table", "cfg." .. variant .. " is a table")
  assert_true(type(v.workflow) == "table",
    "cfg." .. variant .. ".workflow is a table")
  assert_true(type(v.workflow.role_models) == "table",
    "cfg." .. variant .. ".workflow.role_models is a table")
  return v.workflow.role_models
end

-- prod: every role pinned to a Nestor model identifier.
local prod_models = role_models_of("prod")
for _, role in ipairs(team_roles) do
  assert_true(prod_models[role] ~= nil,
    "cfg.prod.workflow.role_models." .. role .. " must be set")
end

-- test: every role pinned to an ollama model identifier.
local test_models = role_models_of("test")
for _, role in ipairs(team_roles) do
  assert_true(test_models[role] ~= nil,
    "cfg.test.workflow.role_models." .. role .. " must be set")
end

-- mock: empty by design — mock-plugin dispatches by prompt content,
-- not model name.
local mock_models = role_models_of("mock")
local mock_count = 0
for _ in pairs(mock_models) do mock_count = mock_count + 1 end
assert_eq(mock_count, 0,
  "cfg.mock.workflow.role_models must stay empty")

local function assert_strings_non_empty(map, label)
  for role, value in pairs(map) do
    assert_true(type(value) == "string",
      label .. "." .. tostring(role) .. " must be a string (got "
      .. type(value) .. ")")
    assert_true(#value > 0,
      label .. "." .. tostring(role) .. " must be non-empty")
  end
end

assert_strings_non_empty(prod_models, "cfg.prod.workflow.role_models")
assert_strings_non_empty(test_models, "cfg.test.workflow.role_models")

-- Shared workflow keys (concurrency / enabled) survive the per-variant
-- workflow_with() shallow merge so a future config edit can't silently
-- drop them.
for _, variant in ipairs({ "prod", "test", "mock" }) do
  local wf = cfg_module[variant].workflow
  assert_true(wf.enabled ~= nil,
    "cfg." .. variant .. ".workflow.enabled must be preserved")
  assert_true(type(wf.concurrency) == "number",
    "cfg." .. variant .. ".workflow.concurrency must be a number")
end
