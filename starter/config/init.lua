-- starter/config/init.lua — settings table for the nefor-team
-- composition. Switch with NEFOR_CONFIG=<variant>; default is prod.
--
-- Variants:
--   * prod    — DP→JWT auth + openai-provider against Nestor + qwen
--               think-tag filter. Requires DP credentials.
--   * test    — openai-provider against local ollama with qwen2.5:7b.
--               No auth, no filter. For machines without Nestor access.
--   * dev     — alias for test.
--   * staging — alias for prod.

local M = {}
local H = {}

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

local function env_or(name, fallback)
  local v = os.getenv(name)
  if type(v) == "string" and #v > 0 then return v end
  return fallback
end

-- Test/dev (ollama) defaults — overridable via env so the user doesn't
-- have to edit this file to swap which local model nefor talks to.
local TEST_MODEL    = env_or("NEFOR_OLLAMA_MODEL", "qwen2.5:7b")
local TEST_BASE_URL = env_or("NEFOR_OLLAMA_BASE_URL", "http://localhost:11434")

-- 0.4 MAG orchestration profiles. Models stay provider-level: profiles only
-- select provider/model defaults and reasoning depth; roles do not pin models.
local function orchestration_profiles(provider)
  return {
    fast     = { provider = provider, reasoning_effort = "low" },
    standard = { provider = provider, reasoning_effort = "medium" },
    deep     = { provider = provider, reasoning_effort = "high" },
    max      = { provider = provider, reasoning_effort = "xhigh" },
  }
end

-- Tool-gate policy is shared across variants — environments only differ
-- by which provider/model they point at. The gate is part of the
-- product, not a per-environment knob.
--
-- Default stance: read/context and orchestration-control tools are auto.
-- Write-capable work is gated at MAG execute by write-review approval; bash
-- still goes through a prompt/validator path because shell policy is runtime-
-- dependent. default_action stays prompt so unfamiliar future tools surface.
local SHARED_TOOL_GATE = {
  default_action = "prompt",
  auto_tools = {
    "read_file",
    "list_dir",
    "search_text",
    "skill",
    "discover_instruction_files",
    "write-review",
    "submit-plan",
    "graph-status",
    "terminate-graph",
    "mag",
    "mag-eval",
  },
  prompt_tools = { "bash" },
}
H.shared_tool_gate = SHARED_TOOL_GATE
H.env_or = env_or
H.orchestration_profiles = orchestration_profiles

M.prod = {
  default_provider         = "nestor",
  -- Prod pins the exact model. Boot resolution (see init.lua pick_model):
  -- NEFOR_TEAM_MODEL env -> a concrete default_model (this pin) -> API
  -- is_default -> first API model. The "default" sentinel would instead
  -- defer to the API's own default rather than pinning.
  default_model            = "tgpt/qwen35-397b-a17b-fp8",
  default_reasoning_effort = "medium",
  lead_reasoning_effort    = "medium",
  providers = {
    nestor = {
      kind = "nestor",
      name = "nestor",
    },
  },
  orchestration_profiles = orchestration_profiles("nestor"),
  tool_gate = SHARED_TOOL_GATE,
  log_level = "info",
}

M.test = {
  default_provider         = "ollama",
  default_model            = TEST_MODEL,
  default_reasoning_effort = "medium",
  lead_reasoning_effort    = "medium",
  providers = {
    ollama = {
      kind     = "ollama",
      name     = "ollama",
      base_url = TEST_BASE_URL,
    },
  },
  orchestration_profiles = orchestration_profiles("ollama"),
  tool_gate = SHARED_TOOL_GATE,
  log_level = "info",
}

M.dev     = M.test
M.staging = M.prod

-- Optional untracked local variants. Kept as a tiny compatibility seam for
-- developer-only providers while preserving the 0.4 config shape. Loaded via
-- loadfile (not require) so the chunk actually receives the helper table H —
-- standard `require` ignores extra arguments, so `require("config.local", H)`
-- would never deliver the helpers. Absence is fine; a genuine error in
-- local.lua surfaces loudly instead of being swallowed by pcall(require, ...).
do
  local path = package.searchpath("config.local", package.path)
  if path then
    local chunk, load_err = loadfile(path)
    if not chunk then
      error("config/local.lua failed to parse: " .. tostring(load_err))
    end
    local ok, local_cfg = pcall(chunk, H)
    if not ok then
      error("config/local.lua failed to run: " .. tostring(local_cfg))
    end
    if type(local_cfg) == "table" then
      local variants = type(local_cfg.variants) == "table" and local_cfg.variants or local_cfg
      for name, variant in pairs(variants) do
        if type(variant) == "table" then
          M[name] = variant
        end
      end
    end
  end
end

local variant = os.getenv("NEFOR_CONFIG")
if variant == nil or variant == "" then
  variant = "prod"
end

M.active = M[variant] or error("unknown NEFOR_CONFIG: " .. tostring(variant))
M.variant = variant

return M
