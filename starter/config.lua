-- starter/config.lua — settings table for this composition. Edit values, not call sites.
--
-- Switch with NEFOR_CONFIG=test (or dev/staging — see aliases below).
-- Default is prod when the env var is unset.
--
-- USE_MOCK_PROVIDER=true is deprecated; if set without NEFOR_CONFIG it
-- maps to the `test` table and emits a one-line warning via nefor.log.

local M = {}

-- Shared model fragment — the upstream starter has no Ollama model
-- pinned (operator picks one in a fork or via PROVIDER_MODEL). Keeping
-- this nil mirrors the prior behavior; openai-provider's spawn omits
-- --model when nil.
M.prod = {
  provider = {
    name         = "ollama",
    model        = nil,
    static_token = "ollama-local",
    base_url     = "http://localhost:11434",
    extra_args   = {},
  },
  plugins = {
    spawn_mock = false,
  },
  tool_gate = {
    -- TUI surface has the permission popup, so prompt is the safe default.
    default_action = "prompt",
    prompt_tools   = { "read_file" },
  },
  log_level = "info",
}

M.test = {
  provider = {
    name        = "mock-plugin",
    model       = "mock-model",
    -- Resolved against STARTER_ROOT in init.lua at load time.
    mock_script = "mock_provider.lua",
  },
  plugins = {
    spawn_mock = true,
  },
  tool_gate = {
    -- Mock runs under CI/repro harnesses with no popup UI — auto keeps
    -- the agent unblocked end-to-end.
    default_action = "auto",
    prompt_tools   = {},
  },
  log_level = "warn",
}

-- Aliases — semantic names that map to the same tables.
M.dev     = M.test  -- mock provider, fast iteration
M.staging = M.prod  -- prod composition, just on a non-production machine

-- Resolve. Single switch read at load time.
local explicit       = os.getenv("NEFOR_CONFIG")
local legacy_mock    = (os.getenv("USE_MOCK_PROVIDER") == "true")
local DEPRECATION_MSG = "USE_MOCK_PROVIDER=true is deprecated; use NEFOR_CONFIG=test instead"

local variant
if explicit and explicit ~= "" then
  variant = explicit
elseif legacy_mock then
  variant = "test"
  -- nefor.log may not be installed yet when this file loads under
  -- non-engine test harnesses; guard the call.
  if type(nefor) == "table" and type(nefor.log) == "table"
      and type(nefor.log.warn) == "function" then
    nefor.log.warn(DEPRECATION_MSG)
  else
    io.stderr:write("[nefor] " .. DEPRECATION_MSG .. "\n")
  end
else
  variant = "prod"
end

M.active = M[variant] or error("unknown NEFOR_CONFIG: " .. tostring(variant))
M.variant = variant

return M
