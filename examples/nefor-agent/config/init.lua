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

local transport_retry = {
  max_attempts = 8,
  max_rounds = 6,
  backoff_multiplier = 2,
  initial_delay_ms = 500,
  max_delay_ms = 30000,
  round_cooldown_ms = 60000,
  jitter_bps = 1000,
  response_headers_timeout_ms = 120000,
}

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
    -- The interactive demo behavior; relative paths resolve against the
    -- starter root. NEFOR_MOCK_PROVIDER_SCRIPT selects another behavior
    -- script, e.g. a test's own scenario.
    mock_script = os.getenv("NEFOR_MOCK_PROVIDER_SCRIPT") or "mock-provider/init.lua",
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
    transport_retry = transport_retry,
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
    transport_retry = transport_retry,
    name         = "ollama",
    static_token = "ollama-local",
    base_url     = "http://localhost:11434",
    extra_args   = {},
  }
end

M.active = {
  compaction_policy = { kind = "Disabled" },
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

  log_level = "info",
}

return M
