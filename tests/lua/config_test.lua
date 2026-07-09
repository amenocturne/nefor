-- tests/lua/config_test.lua — variant-table assertions for config.
--
-- 0.4 config keeps model selection provider-level. Sub-agent roles do
-- not pin models; they inherit the default model chosen by starter/init.lua.
--
-- The config module reads NEFOR_CONFIG at module-load to pick M.active.
-- The tests below read M.prod / M.test directly so the env-var
-- resolution path is irrelevant.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local lead_role = require("lead-workflow.role")

local team_roles = {
  "explorer", "worker", "reviewer", "docs", "critic",
}

for _, role in ipairs(team_roles) do
  local role_cfg = lead_role.AGENT_CONFIGS[role]
  assert_true(role_cfg ~= nil,
    "lead-workflow.role AGENT_CONFIGS." .. role .. " must exist")
  assert_true(role_cfg.model == nil,
    "lead-workflow.role AGENT_CONFIGS." .. role .. ".model must not be pinned")
end

local cfg_module = require("config")

local required_top_level = {
  "default_provider",
  "default_model",
  "default_reasoning_effort",
  "lead_reasoning_effort",
  "providers",
  "orchestration_profiles",
  "tool_gate",
  "log_level",
}

for _, variant in ipairs({ "prod", "test" }) do
  local cfg = cfg_module[variant]
  assert_true(type(cfg) == "table", "cfg." .. variant .. " is a table")
  for _, key in ipairs(required_top_level) do
    assert_true(cfg[key] ~= nil, "cfg." .. variant .. "." .. key .. " must be set")
  end
  assert_true(type(cfg.providers[cfg.default_provider]) == "table",
    "cfg." .. variant .. ".providers[default_provider] must exist")
  for _, profile_name in ipairs({ "fast", "standard", "deep", "max" }) do
    local profile = cfg.orchestration_profiles[profile_name]
    assert_true(type(profile) == "table",
      "cfg." .. variant .. ".orchestration_profiles." .. profile_name .. " must be a table")
    assert_true(type(profile.provider) == "string",
      "cfg." .. variant .. ".orchestration_profiles." .. profile_name .. ".provider must be a string")
    assert_true(type(profile.reasoning_effort) == "string",
      "cfg." .. variant .. ".orchestration_profiles." .. profile_name .. ".reasoning_effort must be a string")
  end
  for _, tool in ipairs({ "mag", "mag-eval", "write-review", "graph-status", "terminate-graph" }) do
    local found = false
    for _, configured in ipairs(cfg.tool_gate.auto_tools or {}) do
      if configured == tool then found = true end
    end
    assert_true(found, "cfg." .. variant .. ".tool_gate.auto_tools contains " .. tool)
  end
end

-- chatgpt_local is a developer-only variant loaded from the untracked,
-- gitignored config/local.lua. Assert its shape only when it is present so a
-- clean checkout / CI (no local.lua) still passes.
if cfg_module.chatgpt_local ~= nil then
  for _, profile_name in ipairs({ "fast", "standard", "deep", "max" }) do
    assert_true(type(cfg_module.chatgpt_local.orchestration_profiles[profile_name]) == "table",
      "cfg.chatgpt_local.orchestration_profiles." .. profile_name .. " must be a table")
  end
end

assert_true(cfg_module.mock == nil, "cfg.mock must not be defined")
assert_true(cfg_module.dev == cfg_module.test, "cfg.dev aliases cfg.test")
assert_true(cfg_module.staging == cfg_module.prod, "cfg.staging aliases cfg.prod")
