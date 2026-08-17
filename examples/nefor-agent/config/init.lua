-- config/init.lua — starter defaults.
--
-- Providers:
--   * mock    — default local scripted provider.
--   * chatgpt — opt in with NEFOR_ENABLE_CHATGPT=1.
--   * ollama  — opt in with NEFOR_ENABLE_OLLAMA=1; openai-provider
--               against http://localhost:11434.
--   * openrouter — opt in with NEFOR_ENABLE_OPENROUTER=1; authentication
--                  uses openai-provider's OPENAI_PROVIDER_API_KEY input.
--
-- Enabled providers register on the bus, so the `/model` picker shows
-- entries from each.

local M = {}

local DEFAULT_REASONING_EFFORT = "xhigh"

local function env_truthy(name)
  local v = os.getenv(name)
  return v == "1" or v == "true" or v == "TRUE" or v == "yes" or v == "YES"
end

local distribution = require("config.distribution")

M.bin = distribution.binary

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
    usage = {
      subscribe = {
        subscription_id = "example-statusline",
        usage_ids = { "chatgpt/subscription", "openrouter/session-total" },
      },
      exposures = {
        { usage_id = "chatgpt/subscription", initial = { kind = "unknown" } },
      },
      subscription = {
        usage_id = "chatgpt/subscription",
        request_kind = "chatgpt.usage.requested",
        updated_kind = "chatgpt.usage.updated",
        error_kind = "chatgpt.usage.error",
        extract = function(snapshot)
          local windows = {}
          for _, entry in ipairs({
            { key = "primary_window", id = "primary" },
            { key = "secondary_window", id = "secondary" },
          }) do
            local window = snapshot.rate_limit and snapshot.rate_limit[entry.key]
            if type(window) == "table" then
              windows[#windows + 1] = { id = entry.id,
                used_percent = window.used_percent,
                window_seconds = window.limit_window_seconds,
                reset_at = window.reset_at }
            end
          end
          return { kind = "subscription", plan = snapshot.plan_type,
            windows = windows, credits = snapshot.credits }
        end,
      },
    },
  }
end

if env_truthy("NEFOR_ENABLE_OPENROUTER") then
  providers[#providers + 1] = {
    kind = "openai",
    name = "openrouter",
    base_url = "https://openrouter.ai/api",
    extra_args = {},
    -- The provider reads OPENAI_PROVIDER_API_KEY; set it to the OpenRouter key
    -- when this instance is enabled.
    request_additions = { usage = { include = true } }, -- OpenRouter cost opt-in
    usage = {
      subscribe = {
        subscription_id = "example-statusline",
        usage_ids = { "chatgpt/subscription", "openrouter/session-total" },
      },
      exposures = {
        { usage_id = "openrouter/session-total", initial = { kind = "unknown" } },
      },
      contributions = {
        {
          usage_id = "openrouter/session-total",
          extension = "cost",
          byok_extension = "is_byok",
          currency = "USD",
          event_kind = "openrouter.usage.cost_contribution.recorded",
        },
      },
    },
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

  usage = {
    command_ids = { "chatgpt/subscription", "openrouter/session-total" },
    statusline_ids = { "chatgpt/subscription", "openrouter/session-total" },
    -- Account values survive an in-process session switch; session values are
    -- cleared until their owning provider reconstructs or reports them.
    account_ids = { "chatgpt/subscription" },
    session_ids = { "openrouter/session-total" },
  },
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
