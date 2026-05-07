-- starter/config.lua — settings table for this composition. Edit values, not call sites.
--
-- Switch with NEFOR_CONFIG=prod (or staging, which aliases to prod).
-- Default is `test` when the env var is unset — `nefor --config ./starter`
-- launches the deterministic mock provider out of the box so a developer
-- testing interactively gets a self-documenting test machine without an
-- external LLM. Set NEFOR_CONFIG=prod explicitly to switch to the real
-- ollama-backed composition.
--
-- USE_MOCK_PROVIDER=true is deprecated; if set without NEFOR_CONFIG it
-- maps to the `test` table and emits a one-line warning via nefor.log.

local M = {}

-- Binary path resolver. Plugins call `require("config").bin("<name>")` to
-- get the absolute path of a sibling plugin binary; the engine sets
-- NEFOR_PLUGIN_DIR before any Lua runs (resolved from the engine's
-- install layout — see crates/nefor/src/main.rs).
M.bin = function(name)
  local plugin_dir = os.getenv("NEFOR_PLUGIN_DIR")
  if not plugin_dir or plugin_dir == "" then
    error("NEFOR_PLUGIN_DIR is not set; the engine resolves this "
       .. "automatically when started via `nefor`. If you see this "
       .. "from a custom harness, set it explicitly or pass --plugin-dir.")
  end
  return plugin_dir .. "/" .. name
end

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
  -- Default flip (qol-fixes): unset NEFOR_CONFIG now resolves to `test`
  -- so the bundled starter is a deterministic mock by default. The
  -- prod composition stays available via NEFOR_CONFIG=prod.
  variant = "test"
end

M.active = M[variant] or error("unknown NEFOR_CONFIG: " .. tostring(variant))
M.variant = variant

return M
