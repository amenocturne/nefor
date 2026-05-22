-- starter/config/init.lua — settings table for the nefor-team
-- composition. Switch with NEFOR_CONFIG=<variant>; default is prod.
--
-- Variants:
--   * prod    — DP→JWT auth + openai-provider against Nestor + qwen
--               think-tag filter. Requires DP credentials.
--   * test    — openai-provider against local ollama with qwen2.5:7b.
--               No auth, no filter. For machines without Nestor access.
--   * mock    — mock-plugin scripted from upstream's mock-provider.
--               Deterministic; for CI / repro harnesses.
--   * dev     — alias for test.
--   * staging — alias for prod.

local M = {}

-- Plugins call `require("config").bin("<name>")` to get the absolute
-- path of a sibling plugin binary; the engine sets NEFOR_PLUGIN_DIR
-- before any Lua runs.
M.bin = function(name)
  local plugin_dir = os.getenv("NEFOR_PLUGIN_DIR")
  if not plugin_dir or plugin_dir == "" then
    error("NEFOR_PLUGIN_DIR is not set; the engine resolves this "
       .. "automatically when started via `nefor`. If you see this "
       .. "from a custom harness, set it explicitly or pass --plugin-dir.")
  end
  return plugin_dir .. "/" .. name
end

-- Lead-workflow defaults shared by every variant. Per-variant tables
-- can override any key by passing `workflow = workflow_with(...)`. Lua
-- merges shallowly; keys not set in the override fall back to here.
local DEFAULT_WORKFLOW = {
  enabled     = true,
  -- Concurrency cap on simultaneously-firing agent nodes. The
  -- lead-workflow actor batches dispatches to honour this.
  concurrency = 3,
  -- Per-role model overrides. Each variant fills its own table below;
  -- nil entries fall back to the session default (whatever the boot
  -- picker chose via NEFOR_TEAM_MODEL / API default).
  role_models = {},
}

-- Roster keys must match lead-workflow/role.lua's AGENT_CONFIGS and the
-- team_roles list in tests/lua/config_test.lua. Both variants currently
-- run every role on the same model; once smaller/faster models become
-- available, hand-pin per role here and the table stops being uniform.
local function all_roles(model)
  return {
    explorer            = model,
    builder             = model,
    reviewer            = model,
    tester              = model,
    critic              = model,
    reflector           = model,
    ["prompt-engineer"] = model,
    docs                = model,
  }
end

-- Prod: Nestor cluster's qwen35-397b.
local PROD_ROLE_MODELS = all_roles("tgpt/qwen35-397b-a17b-fp8")

-- Test (ollama) defaults — overridable via env so the user doesn't have
-- to edit this file to swap which local model nefor talks to. The role
-- list applies the same override so every sub-agent runs on the same
-- model the lead does (matches the prod-side single-model assumption).
local function env_or(name, fallback)
  local v = os.getenv(name)
  if type(v) == "string" and #v > 0 then return v end
  return fallback
end
local TEST_MODEL    = env_or("NEFOR_OLLAMA_MODEL", "qwen2.5:7b")
local TEST_BASE_URL = env_or("NEFOR_OLLAMA_BASE_URL", "http://localhost:11434")
local TEST_ROLE_MODELS = all_roles(TEST_MODEL)

local function workflow_with(role_models)
  local t = {}
  for k, v in pairs(DEFAULT_WORKFLOW) do t[k] = v end
  t.role_models = role_models
  return t
end

-- Tool-gate policy is shared across variants — prod, test, and mock
-- only differ by which provider/model they point at. The gate is part
-- of the product, not a per-environment knob.
--
-- Default stance: every tool in lead_role.TOOL_ALLOWLIST runs --auto
-- (read-only investigation tools never need a popup; writer sub-agents
-- inherit trust from the dispatch-graph approval). prompt_tools flips
-- the two runtime gating points back to --prompt:
--
--   * dispatch-graph — fan-out gate. One click reviews the entire
--     graph the lead is about to run.
--   * bash          — per-command classification via tool-validator
--     (which calls `da`). Safe read-only commands auto-approve;
--     anything else surfaces as a popup.
--
-- default_action stays "prompt" so an unfamiliar tool a future plugin
-- advertises (not in TOOL_ALLOWLIST) surfaces in front of the user
-- instead of running silently.
local SHARED_TOOL_GATE = {
  default_action     = "prompt",
  use_lead_allowlist = true,
  prompt_tools       = { "dispatch-graph", "bash" },
}

M.prod = {
  provider = {
    kind = "nestor",
    name = "nestor",
  },
  tool_gate = SHARED_TOOL_GATE,
  workflow = workflow_with(PROD_ROLE_MODELS),
  log_level = "info",
}

M.test = {
  provider = {
    kind     = "ollama",
    name     = "ollama",
    model    = TEST_MODEL,
    base_url = TEST_BASE_URL,
  },
  tool_gate = SHARED_TOOL_GATE,
  workflow = workflow_with(TEST_ROLE_MODELS),
  log_level = "info",
}

M.mock = {
  provider = {
    kind        = "mock",
    name        = "mock-plugin",
    model       = "mock-model",
    mock_script = "mock-provider/init.lua",
  },
  tool_gate = SHARED_TOOL_GATE,
  -- mock-plugin dispatches by prompt content, not model name, so
  -- role_models stays empty.
  workflow = workflow_with({}),
  log_level = "warn",
}

M.dev     = M.test
M.staging = M.prod

-- Confluence wiki config — not variant-specific.
M.confluence = {
  host = "https://wiki.tcsbank.ru",
}

local variant = os.getenv("NEFOR_CONFIG")
if variant == nil or variant == "" then
  variant = "prod"
end

M.active = M[variant] or error("unknown NEFOR_CONFIG: " .. tostring(variant))
M.variant = variant

return M
