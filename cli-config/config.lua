-- cli-config/config.lua — settings table for the agentic-cli composition.
--
-- Switch with NEFOR_CONFIG=test (or dev/staging — see aliases below).
-- Default is prod when the env var is unset.
--
-- USE_MOCK_PROVIDER=true is deprecated; if set without NEFOR_CONFIG it
-- maps to the `test` table and emits a one-line warning via nefor.log.

local M = {}

-- Binary path resolver. The CLI surface runs against in-tree builds, so
-- we read directly from `<repo>/target/debug/`; the engine still sets
-- NEFOR_PLUGIN_DIR but it points at the source-crate root in this layout
-- (where the binaries don't live), so we don't use it.
do
  local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."
  local PROJECT_ROOT = CONFIG_ROOT:match("^(.*)/[^/]+$") or "."
  M.bin = function(name) return PROJECT_ROOT .. "/target/debug/" .. name end
end

M.prod = {
  provider = {
    name         = "ollama",
    model        = "qwen3.6:35b-a3b-coding-mxfp8",
    static_token = "ollama-local",
    base_url     = "http://localhost:11434",
    extra_args   = {},
  },
  plugins = {
    spawn_mock = false,
  },
  tool_gate = {
    -- CLI surface has no permission-prompt UI in v1. Default `auto`
    -- keeps the agent unblocked. --yolo on agentic-cli is the
    -- documented user override (currently a placeholder).
    default_action = "auto",
    prompt_tools   = {},
  },
  log_level = "info",
}

M.test = {
  provider = {
    name        = "mock-plugin",
    model       = "mock-model",
    -- Resolved against STARTER_ROOT in init.lua at load time.
    mock_script = "mock-provider/init.lua",
  },
  plugins = {
    spawn_mock = true,
  },
  tool_gate = {
    default_action = "auto",
    prompt_tools   = {},
  },
  log_level = "warn",
}

-- Aliases — semantic names that map to the same tables.
M.dev     = M.test  -- mock provider, fast iteration
M.staging = M.prod  -- prod composition, just on a non-production machine

local explicit       = os.getenv("NEFOR_CONFIG")
local legacy_mock    = (os.getenv("USE_MOCK_PROVIDER") == "true")
local DEPRECATION_MSG = "USE_MOCK_PROVIDER=true is deprecated; use NEFOR_CONFIG=test instead"

local variant
if explicit and explicit ~= "" then
  variant = explicit
elseif legacy_mock then
  variant = "test"
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
