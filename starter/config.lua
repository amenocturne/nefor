-- starter/config.lua — settings table for this composition. Edit values, not call sites.
--
-- Two providers are spawned unconditionally:
--   * mock-plugin   — deterministic, works without external deps; the
--                     default so a developer launching `nefor --config
--                     ./starter` always gets a working first turn.
--   * ollama        — openai-provider against http://localhost:11434.
--                     If Ollama isn't running the provider plugin will
--                     fail naturally on first request and surface an
--                     error envelope; we don't probe at startup.
--
-- Both providers register on the bus, so the `/model` picker shows
-- entries from each.

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

M.active = {
  -- Default provider/model used by agentic-loop until the user picks
  -- something else via /model. mock-plugin is the default so the first
  -- turn always works regardless of whether ollama is running.
  default_provider = "mock-plugin",
  default_model    = "mock-model",

  providers = {
    {
      kind        = "mock",
      name        = "mock-plugin",
      -- Resolved against STARTER_ROOT in init.lua at load time.
      mock_script = "mock_provider.lua",
    },
    {
      kind         = "openai",
      name         = "ollama",
      static_token = "ollama-local",
      base_url     = "http://localhost:11434",
      extra_args   = {},
    },
  },

  tool_gate = {
    -- Prompt for tool calls in the TUI. Lead allowlist auto-approves
    -- the canonical reads/writes; everything else gets the popup so a
    -- developer testing the gate path sees the approval flow.
    default_action = "prompt",
    prompt_tools   = { "read_file" },
  },

  log_level = "info",
}

return M
