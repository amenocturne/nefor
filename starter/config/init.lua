-- config/init.lua — starter defaults.
--
-- Providers:
--   * mock    — default local scripted provider.
--   * chatgpt — opt in with NEFOR_ENABLE_CHATGPT=1.
--   * ollama  — opt in with NEFOR_ENABLE_OLLAMA=1; openai-provider
--               against http://localhost:11434.
--
-- Enabled providers register on the bus, so the `/model` picker shows
-- entries from each.

local M = {}

local DEFAULT_REASONING_EFFORT = "xhigh"

local function env_truthy(name)
  local v = os.getenv(name)
  return v == "1" or v == "true" or v == "TRUE" or v == "yes" or v == "YES"
end

local function resolved_bin(name, path, source)
  if nefor and nefor.log and nefor.log.info then
    nefor.log.info("config: resolved plugin binary", {
      name = name,
      path = path,
      source = source,
    })
  end
  return path
end

-- Binary path resolver. Plugins call `require("config").bin("<name>")` to
-- get the absolute path of a sibling plugin binary; the engine sets
-- NEFOR_PLUGIN_DIR before any Lua runs (resolved from the engine's
-- install layout — see crates/nefor/src/main.rs).
--
-- Installed and development launchers both provide the plugin directory
-- explicitly. Runtime resolution never probes mutable source checkouts.
M.bin = function(name)
  local plugin_dir = os.getenv("NEFOR_PLUGIN_DIR")
  if not plugin_dir or plugin_dir == "" then
    error("NEFOR_PLUGIN_DIR is not set; the engine resolves this "
       .. "automatically when started via `nefor`. If you see this "
       .. "from a custom harness, set it explicitly or pass --plugin-dir.")
  end

  return resolved_bin(name, plugin_dir .. "/" .. name, "NEFOR_PLUGIN_DIR")
end

local DEFAULT_PROVIDER = os.getenv("NEFOR_DEFAULT_PROVIDER") or "mock-plugin"
local DEFAULT_MODEL    = os.getenv("NEFOR_DEFAULT_MODEL") or "mock-model"

local providers = {
  {
    kind        = "mock",
    name        = "mock-plugin",
    mock_script = "mock-provider/init.lua",
  },
}

if env_truthy("NEFOR_ENABLE_CHATGPT") then
  providers[#providers + 1] = {
    kind = "chatgpt",
    name = "chatgpt",
  }
end

if env_truthy("NEFOR_ENABLE_OLLAMA") then
  providers[#providers + 1] = {
    kind         = "openai",
    name         = "ollama",
    static_token = "ollama-local",
    base_url     = "http://localhost:11434",
    extra_args   = {},
  }
end

M.active = {
  default_provider = DEFAULT_PROVIDER,
  default_model    = DEFAULT_MODEL,
  default_reasoning_effort = DEFAULT_REASONING_EFFORT,
  lead_reasoning_effort = DEFAULT_REASONING_EFFORT,

  providers = providers,

  orchestration_profiles = {
    fast     = { provider = DEFAULT_PROVIDER, model = DEFAULT_MODEL, reasoning_effort = "low" },
    standard = { provider = DEFAULT_PROVIDER, model = DEFAULT_MODEL, reasoning_effort = "medium" },
    deep     = { provider = DEFAULT_PROVIDER, model = DEFAULT_MODEL, reasoning_effort = "high" },
    max      = { provider = DEFAULT_PROVIDER, model = DEFAULT_MODEL, reasoning_effort = "xhigh" },
  },

  tool_gate = {
    -- Default policy for unlisted tools. `prompt` = popup; user
    -- approves before the call lands.
    default_action = "prompt",
    -- Tools that bypass the popup entirely. Context I/O
    -- (read_file / read_image / instructions) is safe to auto-allow —
    -- nothing on disk changes. mag / mag-eval are control-plane
    -- dispatch: the graphs they run deliver their capability invokes
    -- (bash, …) back through this gate, where policy applies
    -- unchanged, so gating the dispatch call itself is a redundant
    -- click. write-review (alias submit-plan) is
    -- the lead's plan-submission tool: it doesn't perform side
    -- effects, it just parks a plan for the user's /approve, so
    -- gating it behind an approval popup is a redundant click. The
    -- plan still appears in chat as a chat.plan.append entry where
    -- the user accepts/rejects with /approve / /reject.
    auto_tools     = {
      "read_file", "read_image", "python-read", "instructions", "discover_instruction_files",
      "write-review", "submit-plan", "graph-status", "await-run", "terminate-graph",
      "mag", "mag-eval",
    },
    -- Tools that always go through the popup, regardless of default.
    prompt_tools   = {},
  },

  log_level = "info",
}

return M
