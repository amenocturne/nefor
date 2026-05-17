-- starter/lead-workflow/role.lua — team-port lead-workflow role configs.
--
-- Loads each prompts/<role>.md at module-load time and exposes:
--
--   * LEAD_SYSTEM_PROMPT  — string, the lead orchestrator's prompt.
--   * AGENT_CONFIGS       — table keyed by role name; each entry has
--                           { system_prompt, model, tool_allowlist }.
--   * ORCHESTRATION_TOOLS — list of tool names the lead has access to.
--   * TOOL_ALLOWLIST      — union fed to tool-gate's --prompt argv.
--
-- The team port carries seven sub-agent roles (explorer, builder,
-- reviewer, tester, critic, reflector, prompt-engineer) plus per-role
-- model overrides drawn from cfg.workflow.role_models.
--
-- Prompts are read from disk rather than embedded as Lua string
-- literals because long strings inside Lua are painful (escaping, no
-- syntax highlighting in editors, no clean diffs).
--
-- Per-role model resolution: AGENT_CONFIGS[role].model is set from
-- cfg.workflow.role_models[role]; nil falls back to the session default
-- configured via agentic_loop.configure { model = ... }.

local M = {}

-- NEFOR_CONFIG_DIR is the global the engine sets to the directory
-- containing init.lua. For tests that load the module without booting
-- the engine, the test rig sets NEFOR_CONFIG_DIR explicitly.
local STARTER_ROOT = (rawget(_G, "NEFOR_CONFIG_DIR") or ".")
local PROMPTS_DIR  = STARTER_ROOT .. "/prompts"

local function read_prompt(name)
  local path = PROMPTS_DIR .. "/" .. name .. ".md"
  local f, err = io.open(path, "r")
  if not f then
    return nil, "lead-workflow.role: cannot open " .. path .. ": " .. tostring(err)
  end
  local content = f:read("*a")
  f:close()
  if not content or content == "" then
    return nil, "lead-workflow.role: empty prompt at " .. path
  end
  return content
end

-- A missing prompt file is a developer error, not a runtime condition.
-- Surface a placeholder so the module still loads, but make it
-- obviously broken if it ever reaches a model.
local function load_or_placeholder(name)
  local content, err = read_prompt(name)
  if content then return content end
  return "[lead-workflow.role: prompt '" .. name .. "' missing — " .. tostring(err) .. "]"
end

local function load_role_models()
  local cfg_module = require("config")
  local active = cfg_module.active
  if type(active) ~= "table" then return {} end
  local workflow = active.workflow
  if type(workflow) ~= "table" then return {} end
  local models = workflow.role_models
  if type(models) ~= "table" then return {} end
  return models
end

local ROLE_MODELS = load_role_models()
local function model_for(role) return ROLE_MODELS[role] end

M.LEAD_SYSTEM_PROMPT = load_or_placeholder("lead")

-- basic-tools advertises read_file, write_file, bash. read-only-tools
-- (Lua-resident) adds list_dir + search_text — actual read-only
-- primitives for investigation, replacing the previous explorer-with-
-- bash shape (bash is a sandbox-escape hatch via shell composition,
-- so "read-only role with bash" was a contradiction).
--
-- The synthetic `finalize` terminator is appended by the agent reasoner
-- itself; it does not need listing here.
--
-- Per-role boundaries:
--   * explorer/reviewer/critic/reflector — read-only investigation
--                       (read_file + list_dir + search_text). No shell,
--                       no write.
--   * builder         — read-only set + write_file + bash.
--   * tester          — read-only set + bash (runs the test command).
--   * prompt-engineer — read-only set + write_file (writes prompt
--                       files; no bash).
M.AGENT_CONFIGS = {
  explorer = {
    system_prompt  = load_or_placeholder("explorer"),
    model          = model_for("explorer"),
    tool_allowlist = { "read_file", "list_dir", "search_text", "bash" },
    read_only      = true,
  },
  builder = {
    system_prompt  = load_or_placeholder("builder"),
    model          = model_for("builder"),
    tool_allowlist = { "read_file", "list_dir", "search_text", "write_file", "bash" },
    read_only      = false,
  },
  reviewer = {
    system_prompt  = load_or_placeholder("reviewer"),
    model          = model_for("reviewer"),
    tool_allowlist = { "read_file", "list_dir", "search_text" },
    read_only      = true,
  },
  tester = {
    system_prompt  = load_or_placeholder("tester"),
    model          = model_for("tester"),
    tool_allowlist = { "read_file", "list_dir", "search_text", "bash" },
    read_only      = false,
  },
  critic = {
    system_prompt  = load_or_placeholder("critic"),
    model          = model_for("critic"),
    tool_allowlist = { "read_file", "list_dir", "search_text" },
    read_only      = true,
  },
  reflector = {
    system_prompt  = load_or_placeholder("reflector"),
    model          = model_for("reflector"),
    tool_allowlist = { "read_file", "list_dir", "search_text" },
    read_only      = true,
  },
  ["prompt-engineer"] = {
    system_prompt  = load_or_placeholder("prompt-engineer"),
    model          = model_for("prompt-engineer"),
    tool_allowlist = { "read_file", "list_dir", "search_text", "write_file" },
    read_only      = false,
  },
}

-- The lead does NOT get read/grep/find/ls/glob/write/edit/bash directly
-- — investigation goes through explorer nodes, changes through builder
-- nodes.
M.ORCHESTRATION_TOOLS = {
  "read_file",
  "dispatch-graph",
  "write-review",
  "progress",
  "critique",
  "terminate",
}

-- TOOL_ALLOWLIST — union of every role's tool surface plus the lead's
-- orchestration tools. Fed into tool-gate's `--prompt <name>` argv.
-- Including orchestration names here is harmless: tool-gate is happy
-- to allowlist names that no Rust plugin advertises.
do
  local seen = {}
  local union = {}
  local function add(t)
    if not seen[t] then seen[t] = true; union[#union + 1] = t end
  end
  for _, role in pairs(M.AGENT_CONFIGS) do
    for _, t in ipairs(role.tool_allowlist) do add(t) end
  end
  for _, t in ipairs(M.ORCHESTRATION_TOOLS) do add(t) end
  M.TOOL_ALLOWLIST = union
end

return M
