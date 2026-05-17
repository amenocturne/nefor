-- starter/lead-workflow/role.lua — lead-workflow role configs for the
-- starter.
--
-- Loads each `prompts/<role>.md` at module-load time and exposes:
--
--   * LEAD_SYSTEM_PROMPT  — string, the lead orchestrator's prompt.
--   * AGENT_CONFIGS       — table keyed by role name; each entry has
--                           { system_prompt, model, tool_allowlist }.
--   * ORCHESTRATION_TOOLS — list of tool names the lead has access to.
--
-- The team port (`nefor-team`) replaces this file with a richer
-- version that adds more roles, work-specific phrasing, and per-role
-- model pinning. The starter ships a deliberately small surface (lead
-- + explorer + builder + reviewer) so the public starter runs cleanly
-- against the mock and ollama providers without dragging in
-- team-specific tooling references.
--
-- `model = nil` on every role lets the agent reasoner fall back to
-- `state.config.model` (set via `agentic_loop.configure`).
--
-- Prompts are read from disk rather than embedded as Lua string
-- literals because long strings inside Lua are painful (escaping, no
-- syntax highlighting in editors, no clean diffs).

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

-- Reasoning-hygiene preamble. Prepended to every role prompt so every
-- system prompt the engine ships starts with this discipline.
--
-- WHY: Qwen 3 / Ollama-class thinking models route reasoning vs
-- content via raw `<think>...</think>` tags in the chat template.
-- When the model writes the literal characters `</think>` inside its
-- reasoning (because it's discussing tag handling, escaping, or its
-- own format), the chat-template parser sees the close tag and ends
-- the reasoning channel mid-thought. The model's continuing reasoning
-- then bleeds into the content channel as user-visible answer text.
-- We've observed this in production: a 30-line internal monologue
-- collapsed onto the user's screen as if it were a final answer.
--
-- Telling the model to refer to the tag descriptively ("the closing
-- think tag") rather than reproducing the literal characters defuses
-- the parser. This is a mitigation, not a fix — the underlying
-- chat-template behaviour belongs to Ollama — but it removes the
-- common trigger.
local REASONING_HYGIENE = table.concat({
  "## Reasoning channel hygiene",
  "",
  "If you reason about your own output format — thinking tags, end-of-",
  "reasoning markers, channel separators — DO NOT reproduce the literal",
  "tag characters in your reasoning. Refer to them descriptively (e.g.",
  '"the closing think tag", "the end-of-reasoning marker") instead of',
  "writing the tag verbatim. Writing the literal close-tag characters in",
  "your reasoning causes the chat-template parser to end the reasoning",
  "channel where you wrote them, and the rest of your thought leaks",
  "into the user-visible answer.",
  "",
  "---",
  "",
}, "\n")

-- Failure mode: a missing prompt file is a developer error, not a
-- runtime condition. We surface a placeholder string here so the
-- module still loads (downstream code can detect the placeholder and
-- decide what to do) but the placeholder is obviously broken if it
-- ever reaches a model.
local function load_or_placeholder(name)
  local content, err = read_prompt(name)
  if content then return REASONING_HYGIENE .. content end
  return "[lead-workflow.role: prompt '" .. name .. "' missing — " .. tostring(err) .. "]"
end

M.LEAD_SYSTEM_PROMPT = load_or_placeholder("lead")

M.AGENT_CONFIGS = {
  explorer = {
    system_prompt  = load_or_placeholder("explorer"),
    model          = nil,
    tool_allowlist = { "read_file", "list_dir", "search_text", "bash" },
    read_only      = true,
  },
  builder = {
    system_prompt  = load_or_placeholder("builder"),
    model          = nil,
    tool_allowlist = { "read_file", "list_dir", "search_text", "write_file", "bash" },
    read_only      = false,
  },
  reviewer = {
    system_prompt  = load_or_placeholder("reviewer"),
    model          = nil,
    tool_allowlist = { "read_file", "list_dir", "search_text" },
    read_only      = true,
  },
}

-- Tools the lead orchestrator has access to. The lead does NOT get
-- read/grep/find/ls/glob/write/edit/bash directly — investigation goes
-- through `explorer` nodes, changes through `builder` nodes. Keep this
-- list minimal in v0.1; the team port adds richer tools (`progress`,
-- `critique`, `terminate`) on top.
M.ORCHESTRATION_TOOLS = {
  "read_file",
  "dispatch-graph",
  "write-review",
}

return M
