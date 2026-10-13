-- cli-config/config.lua — deterministic settings for the agentic-cli example.

local M = {}

-- Binary path resolver. Tests may select Cargo's effective target directory;
-- normal in-tree use keeps the repository's `target/debug` default.
do
  local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."
  local PROJECT_ROOT = CONFIG_ROOT:match("^(.*)/[^/]+$") or "."
  local BIN_ROOT = os.getenv("NEFOR_TEST_BIN_DIR") or (PROJECT_ROOT .. "/target/debug")
  M.bin = function(name) return BIN_ROOT .. "/" .. name end
end

M.active = {
  provider = {
    name        = "mock-plugin",
    model       = "mock-model",
    -- Resolved against STARTER_ROOT in init.lua at load time.
    mock_script = "mock-provider/init.lua",
  },
  log_level = "warn",
}

return M
