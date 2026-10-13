-- Config-selected instruction discovery and skill tools.
--
-- Advertised through tool-gate as source `read-only-tools`. Base tools are
-- opt-in through `build{ include = {...} }`: discover_instruction_files finds
-- repository AGENTS.md/CLAUDE.md paths, while skill loads an ordinary workflow
-- skill from the config. Neither is enabled by default.
--
-- Config-specific tools use `extra_tools = { tool.define { ... } }`, built with
-- `libs.tool`; their `execute` receives validated arguments and settles with
-- call.ok(text) or call.err(message). Included tools and extras share
-- advertisement and dispatch on every live gate hello.

local tool = require("libs.tool")
local envelope = require("core.envelope")
local instruction_files = require("libs.instruction-files")
local tool_display = require("libs.chat.tool_display")

local function field(label, source, path, kind, extra)
  local value = { label = label, select = { source = source, path = path }, kind = kind }
  for key, item in pairs(extra or {}) do value[key] = item end
  return value
end

local function display(label, primary, fields, result_kind, result_text, result_fields, lifecycle)
  return {
    compact = { label = label, primary = primary },
    expanded = { label = label, fields = tool_display.fields(fields) },
    result = { kind = result_kind, text = result_text, fields = tool_display.fields(result_fields) },
    lifecycle = lifecycle,
  }
end

local SOURCE_NAME = "read-only-tools"
local GATE = "tool-gate"

local function emit_result(body)
  local admitted = envelope.checked_emit_as(SOURCE_NAME, nil, body)
  if admitted then return end
  local fallback, err = envelope.checked_emit_as(SOURCE_NAME, nil, { kind = "tool.result", id = body.id,
    error = "read-only-tools: response could not be published", error_code = "output_rejected" })
  if not fallback then error({ kind = "tool_output_unavailable", error = err }, 0) end
end

local function discover_instruction_files(args, call)
  local result = instruction_files.discover(args.path or ".", {
    scope = args.scope or "auto",
    unread_only = args.unread_only == true,
  })
  call.ok(instruction_files.format_discovery(result))
end

-- Load an ordinary workflow skill from the config-owned skills directory.
local SKILLS_DIR = (rawget(_G, "NEFOR_CONFIG_DIR") or ".") .. "/skills"

local function read_one_skill(raw_name)
  local name = raw_name:gsub("/skill%.md$", ""):gsub("%.md$", "")
  local path = SKILLS_DIR .. "/" .. name .. "/skill.md"
  local f, err = io.open(path, "r")
  if not f then
    return nil, tostring(err or ("no skill at " .. path))
  end
  local content = f:read("*a")
  f:close()
  if not content or #content == 0 then
    return nil, "empty skill at " .. path
  end
  return content, nil
end

local function skill(args, call)
  local name = args.name
  if type(name) == "string" then
    local content, err = read_one_skill(name)
    if not content then call.err("skill: " .. err); return end
    call.ok(content)
    return
  end
  if #name == 0 then
    call.err("skill: name must list at least one skill")
    return
  end
  local parts = {}
  for _, n in ipairs(name) do
    local content, err = read_one_skill(n)
    parts[#parts + 1] = "--- skill: " .. n .. " ---\n" ..
      (content or ("[error: " .. tostring(err) .. "]"))
  end
  call.ok(table.concat(parts, "\n\n"))
end

local function base_tools()
  return {
    tool.define {
      name = "discover_instruction_files",
      display = display("discover instructions", field("path", "args", "path", "path", { omit = "missing" }), { field("scope", "args", "scope", "scalar", { omit = "missing" }), field("unread only", "args", "unread_only", "scalar", { omit = "missing" }) }, "content", nil, { field("status", "result", "$", "text", { max_lines = 80, max_bytes = 6400 }) }),
      description =
        "List AGENTS.md and CLAUDE.md instruction files available near " ..
        "a path. Does not read file contents. Use ordinary read_file on " ..
        "any file that seems relevant.",
      parameters = {
        type = "object",
        properties = {
          path = {
            type = "string",
            description = "Directory or file path to inspect. Defaults to '.'.",
          },
          scope = {
            type = "string",
            enum = { "auto", "subfolders" },
            description =
              "auto: git repo when inside one, otherwise subfolders. " ..
              "subfolders: only below path.",
          },
          unread_only = {
            type = "boolean",
            description = "Only show instruction files not read this session.",
          },
        },
      },
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      prepare = tool.nothing,
      execute = discover_instruction_files,
    },
    tool.define {
      name = "skill",
      display = display("load skill", field("source", "args", "name", "list"), {}, "receipt", "skill loaded", { field("status", "result", "$", "status", { sensitive = "omit" }) }),
      description =
        "Load a workflow skill by name from the config's skills directory " ..
        "(" .. SKILLS_DIR .. "/<name>/skill.md). Read the skill BEFORE acting on a " ..
        "task that matches its description — it carries the workflow, CLI " ..
        "usage, and conventions. Pass an array to load several at once.",
      parameters = {
        type = "object",
        properties = {
          name = {
            oneOf = {
              { type = "string" },
              { type = "array", items = { type = "string" } },
            },
            description = "Skill name or array of skill names.",
          },
        },
        required = { "name" },
      },
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      prepare = tool.nothing,
      execute = skill,
    },
  }
end

-- build{ include = { "skill", ... }, extra_tools = { tool.define { ... } } }
--   -> actor spec.
--
-- Base tools are OPT-IN: only the names listed in `include` are registered and
-- advertised. Adding a new base tool to this lib therefore cannot leak into any
-- config until that config lists it — no silent contamination. `extra_tools`
-- registers config-specific tools through the same source.
local function build(opts)
  opts = opts or {}
  local include = opts.include or {}
  local extra_tools = opts.extra_tools or {}

  local base_by_name = {}
  for _, definition in ipairs(base_tools()) do
    base_by_name[definition.name] = definition
  end

  local tools, included = {}, {}
  for _, name in ipairs(include) do
    local definition = base_by_name[name]
    if not definition then
      error("read-only-tools.build: unknown base tool '" .. tostring(name) ..
        "' in include (known: discover_instruction_files, skill)")
    end
    if included[name] then
      error("read-only-tools.build: base tool '" .. name .. "' listed twice in include")
    end
    included[name] = true
    tools[#tools + 1] = definition
  end
  for _, definition in ipairs(extra_tools) do
    tools[#tools + 1] = definition
  end

  return tool.actor {
    source = tool.source { name = SOURCE_NAME, tools = tools, emit = emit_result },
    gate = GATE,
  }
end

return {
  build = build,
}
