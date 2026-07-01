-- config/init.lua — starter defaults.
--
-- Providers:
--   * chatgpt — default; ChatGPT backend, model picked via /model.
--   * ollama  — openai-provider against http://localhost:11434.
--
-- All providers register on the bus, so the `/model` picker shows
-- entries from each.

local M = {}

local DEFAULT_REASONING_EFFORT = "xhigh"

local function path_exists(p)
  local f = io.open(p, "r")
  if f then f:close(); return true end
  return false
end

local function read_nefor_repo()
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local fh = io.open(config_dir .. "/agentic-kit.json", "r")
  if not fh then return nil end
  local raw = fh:read("*a")
  fh:close()
  return raw:match('"nefor_repo"%s*:%s*"([^"]+)"')
end

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
-- Installed configs default to NEFOR_PLUGIN_DIR. A checkout's target/debug
-- can otherwise silently shadow a freshly installed plugin binary and leave
-- live sessions running stale code. Set NEFOR_USE_REPO_PLUGIN_BINS=1 when
-- deliberately testing binaries from agentic-kit.json's nefor_repo checkout.
M.bin = function(name)
  local plugin_dir = os.getenv("NEFOR_PLUGIN_DIR")
  if not plugin_dir or plugin_dir == "" then
    error("NEFOR_PLUGIN_DIR is not set; the engine resolves this "
       .. "automatically when started via `nefor`. If you see this "
       .. "from a custom harness, set it explicitly or pass --plugin-dir.")
  end

  if env_truthy("NEFOR_USE_REPO_PLUGIN_BINS") then
    local nefor_repo = read_nefor_repo()
    if nefor_repo then
      local debug_bin = nefor_repo .. "/target/debug/" .. name
      if path_exists(debug_bin) then
        return resolved_bin(name, debug_bin, "agentic-kit.json nefor_repo target/debug")
      end
      local release_bin = nefor_repo .. "/target/release/" .. name
      if path_exists(release_bin) then
        return resolved_bin(name, release_bin, "agentic-kit.json nefor_repo target/release")
      end
    end
  end

  return resolved_bin(name, plugin_dir .. "/" .. name, "NEFOR_PLUGIN_DIR")
end

M.active = {
  default_provider = os.getenv("NEFOR_DEFAULT_PROVIDER") or "chatgpt",
  default_model    = os.getenv("NEFOR_DEFAULT_MODEL") or "gpt-5.5",
  default_reasoning_effort = DEFAULT_REASONING_EFFORT,
  lead_reasoning_effort = DEFAULT_REASONING_EFFORT,

  providers = {
    {
      kind = "chatgpt",
      name = "chatgpt",
    },
    {
      kind         = "openai",
      name         = "ollama",
      static_token = "ollama-local",
      base_url     = "http://localhost:11434",
      extra_args   = {},
    },
  },

  orchestration_profiles = {
    fast     = { provider = os.getenv("NEFOR_DEFAULT_PROVIDER") or "chatgpt", model = os.getenv("NEFOR_DEFAULT_MODEL") or "gpt-5.5", reasoning_effort = "low" },
    standard = { provider = os.getenv("NEFOR_DEFAULT_PROVIDER") or "chatgpt", model = os.getenv("NEFOR_DEFAULT_MODEL") or "gpt-5.5", reasoning_effort = "medium" },
    deep     = { provider = os.getenv("NEFOR_DEFAULT_PROVIDER") or "chatgpt", model = os.getenv("NEFOR_DEFAULT_MODEL") or "gpt-5.5", reasoning_effort = "high" },
    max      = { provider = os.getenv("NEFOR_DEFAULT_PROVIDER") or "chatgpt", model = os.getenv("NEFOR_DEFAULT_MODEL") or "gpt-5.5", reasoning_effort = "xhigh" },
  },

  tool_gate = {
    -- Default policy for unlisted tools. `prompt` = popup; user
    -- approves before the call lands.
    default_action = "prompt",
    -- Tools that bypass the popup entirely. Read-only investigation
    -- (read_file / read_image / list_dir / search_text / python-read /
    -- instructions) is safe to auto-allow — nothing on disk changes.
    -- write-review (alias submit-plan) is
    -- the lead's plan-submission tool: it doesn't perform side
    -- effects, it just parks a plan for the user's /approve, so
    -- gating it behind an approval popup is a redundant click. The
    -- plan still appears in chat as a chat.plan.append entry where
    -- the user accepts/rejects with /approve / /reject.
    auto_tools     = {
      "read_file", "read_image", "list_dir", "search_text", "python-read", "instructions", "discover_instruction_files",
      "write-review", "submit-plan", "graph-status", "terminate-graph",
      "mag", "mag-env",
    },
    -- Tools that always go through the popup, regardless of default.
    prompt_tools   = {},
  },

  log_level = "info",
}

return M
