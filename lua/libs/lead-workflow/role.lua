-- libs/lead-workflow/role.lua — lead-workflow role loader (mechanism).
--
-- Loads `prompts/lead.md` and `prompts/worker.md` at module-load time and
-- exposes each as a COMPLETE system prompt, read verbatim off disk:
--
--   * LEAD_SYSTEM_PROMPT   — the root lead's full system prompt.
--   * WORKER_SYSTEM_PROMPT — a delegated agent's full system prompt.
--
-- These are final files: the loom prompt compiler (agentic-kit) composes
-- profile fragments + the role overlay + any provider-conditional preamble
-- (the Qwen/Ollama reasoning-hygiene mitigation that used to live here as a
-- Lua string literal) into each file. This loader does NO text composition —
-- it loads and returns. Downstream configs that ship their own prompt files
-- (the starter) fold any provider preamble into those files directly.
--
-- The lead's tool surface is NOT here: it is authored inside the
-- turn-program (`:tools` in agentic-loop/lead-turn.mag).
--
-- Prompts are read from disk rather than embedded as Lua string
-- literals because long strings inside Lua are painful (escaping, no
-- syntax highlighting in editors, no clean diffs).
--
-- PROMPT-ROOT SEAM: this loader is mechanism, but the prompt content it
-- reads is config-owned opinion (`<config>/prompts/*.md`). It resolves
-- the prompt dir from the `NEFOR_CONFIG_DIR` global the engine sets to the
-- dir holding `init.lua` — NOT from this file's own location. So the loader
-- runs unchanged from the shared lua tree while every config keeps its own
-- persona prompt. Downstream configs `require("libs.lead-workflow.role")`;
-- the starter keeps a re-export shim only because starter/init.lua still
-- says `require("lead-workflow.role")`.

local M = {}

-- Resolve the starter root the same way the rest of the starter does:
-- `NEFOR_CONFIG_DIR` is the canonical global the engine sets to the
-- directory containing `init.lua`. For tests that load the module
-- directly without booting the full engine, `package.path` is set so
-- `require("lead-workflow.role")` resolves, and the test rig sets
-- `NEFOR_CONFIG_DIR` to the starter dir.
local STARTER_ROOT = (rawget(_G, "NEFOR_CONFIG_DIR") or ".")
local PROMPTS_DIR = STARTER_ROOT .. "/prompts"

-- Read a prompt file by role name. Returns the file contents on
-- success, or `nil, err_string` on failure. The loader keeps the file
-- handle scoped to this function so an open-and-forget can't leak.
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

-- Failure mode: a missing prompt file is a developer error, not a
-- runtime condition. We surface a placeholder string here so the
-- module still loads (downstream code can detect the placeholder and
-- decide what to do) but the placeholder is obviously broken if it
-- ever reaches a model.
local function load_or_placeholder(name)
  local content, err = read_prompt(name)
  if content then return content end
  return "[lead-workflow.role: prompt '" .. name .. "' missing — " .. tostring(err) .. "]"
end

M.LEAD_SYSTEM_PROMPT = load_or_placeholder("lead")
M.WORKER_SYSTEM_PROMPT = load_or_placeholder("worker")

return M
