-- Ordered model context for MAG-capable agents.
--
-- Configuration owns every guide, module root, public module, toolset
-- manifest, and trailing ambient section. This module only reads that immutable
-- selection once and composes it after an agent-authored system prompt. Tool
-- schemas remain provider-native data.
--
--   guides            { { title, path }, ... } embedded whole, in order
--   book_path         path named as the full reference
--   module_roots      { { name, path }, ... } the roots the compiler searches;
--                     they give each public module its import name
--   public_modules    { path | { path, embed = false }, ... } the MAG sources
--                     authors write against, in order: embedded verbatim by
--                     default, or listed with their path only
--   toolsets          { manifest = path, names = { { key, name }, ... } }  (each toolset is rendered with its approval policy)
--   trailing_sections { string, ... } appended last
--
-- Only public modules are named in the prompt. Every other module under the
-- roots stays importable but is not advertised.

local M = {}
local Context = {}
Context.__index = Context

local REASONER_MENTAL_MODEL = [[# Reasoner mental model

A reasoner has the typed shape `Context -> Context`. Agents, deterministic
commands, human decisions, and complete workflows can all be reasoners. A graph
of reasoners is itself a reasoner: package a complicated subgraph behind typed
inputs and outputs, bind it once, and compose it as a node in a larger graph. In
Nefor, nodes can represent reasoners; MAG itself only manipulates values defined
by libraries and knows neither concept.]]

local function dense_list(value, label)
  if type(value) ~= "table" then error(label .. " must be a list", 3) end
  local count = 0
  for key in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
      error(label .. " must be a dense list", 3)
    end
    count = count + 1
  end
  for index = 1, count do
    if value[index] == nil then error(label .. " must be a dense list", 3) end
  end
  return value
end

local approval = require("libs.tool.approval")

local function nonempty(value, label)
  if type(value) ~= "string" or value == "" then
    error(label .. " must be a non-empty string", 3)
  end
  return value
end

local function read_file(path)
  if nefor and nefor.fs and type(nefor.fs.read_file) == "function" then
    local result = nefor.fs.read_file(path)
    if type(result) == "table" and result.ok and type(result.content) == "string" then
      return result.content
    end
    error("mag-context: cannot read " .. path .. ": " .. tostring(result and result.error), 3)
  end
  local file, open_error = io.open(path, "r")
  if not file then error("mag-context: cannot read " .. path .. ": " .. tostring(open_error), 3) end
  local content = file:read("*a")
  file:close()
  return content
end

local function trim_slashes(path)
  return (path:gsub("/+$", ""))
end

-- A public module's import name is its path below a module root, exactly as
-- the compiler resolves imports: `<root>/a/b.mag` is module `a.b`.
local function module_name(path, roots)
  local extension = path:match("%.magl?$")
  if not extension then
    error("mag-context: public module " .. path .. " is not a .mag or .magl source", 3)
  end
  local matches = {}
  for _, root in ipairs(roots) do
    local prefix = trim_slashes(root.path) .. "/"
    if path:sub(1, #prefix) == prefix then
      matches[#matches + 1] = path:sub(#prefix + 1, -(#extension + 1))
    end
  end
  if #matches ~= 1 then
    local listed = {}
    for _, root in ipairs(roots) do listed[#listed + 1] = root.name .. " = " .. root.path end
    error("mag-context: public module " .. path .. " must be inside exactly one module root ("
      .. (#matches == 0 and "it is inside none" or "it is inside several")
      .. "); module roots: " .. table.concat(listed, ", "), 3)
  end
  for part in (matches[1] .. "/"):gmatch("(.-)/") do
    if part == "" or part:find("[^%w_%-]") then
      error("mag-context: public module " .. path .. " has no valid module name below its root", 3)
    end
  end
  return (matches[1]:gsub("/", "."))
end

local function validated_roots(roots, label)
  local out = {}
  for index, root in ipairs(dense_list(roots or {}, label)) do
    if type(root) ~= "table" then error(label .. " entries must be tables", 3) end
    out[index] = {
      name = nonempty(root.name, label .. "[" .. index .. "].name"),
      path = nonempty(root.path, label .. "[" .. index .. "].path"),
    }
  end
  return out
end

-- An entry is a path (embedded) or { path = ..., embed = false } (listed with
-- its path only). Both forms are read at construction so a wrong path fails
-- at startup instead of leaving a dangling reference in every prompt.
local function public_modules(entries, roots)
  local out, seen = {}, {}
  local label = "mag-context public_modules"
  for index, entry in ipairs(dense_list(entries or {}, label)) do
    local here = label .. "[" .. index .. "]"
    local path, embed = entry, true
    if type(entry) == "table" then
      path = entry.path
      if entry.embed ~= nil and type(entry.embed) ~= "boolean" then
        error(here .. ".embed must be a boolean", 3)
      end
      embed = entry.embed ~= false
      for key in pairs(entry) do
        if key ~= "path" and key ~= "embed" then
          error(here .. " has unknown field " .. tostring(key), 3)
        end
      end
    end
    nonempty(path, type(entry) == "table" and here .. ".path" or here)
    local name = module_name(path, roots)
    if seen[name] then error(here .. " repeats module " .. name, 3) end
    seen[name] = true
    out[index] = { name = name, path = path, embed = embed, source = read_file(path) }
  end
  return out
end

local function fenced(source)
  local longest = 2
  for run in source:gmatch("`+") do
    if #run > longest then longest = #run end
  end
  local fence = string.rep("`", longest + 1)
  return fence .. "mag\n" .. source .. (source:sub(-1) == "\n" and "" or "\n") .. fence
end

local PUBLIC_MODULES_INTRO = [[These are the public MAG modules: the ones meant to be written against. Each is
imported by the module name shown, and descriptions are comments next to its
types and functions. The source of a module marked "source below" follows this
list; a module marked "read when needed" is at the path shown. Modules that are
not listed are internal details behind these.]]

local function render_public_modules(modules)
  if #modules == 0 then return nil end
  local lines = { "## Public modules", "", PUBLIC_MODULES_INTRO, "" }
  for _, module in ipairs(modules) do
    lines[#lines + 1] = "- `" .. module.name .. "` — `" .. module.path .. "` ("
      .. (module.embed and "source below" or "read when needed") .. ")"
  end
  for _, module in ipairs(modules) do
    if module.embed then
      lines[#lines + 1] = ""
      lines[#lines + 1] = "### Module `" .. module.name .. "` — `" .. module.path .. "`"
      lines[#lines + 1] = ""
      lines[#lines + 1] = fenced(module.source)
    end
  end
  return table.concat(lines, "\n")
end

local function decode_json(source)
  if not (nefor and nefor.json and type(nefor.json.decode) == "function") then
    error("mag-context: toolsets require the engine JSON binding", 4)
  end
  return pcall(nefor.json.decode, source)
end

-- The manifest is the configuration's toolsets file that its MAG bindings
-- already read: `{ "<key>": { "policy": "<approval policy>", "tools":
-- ["<tool>", ...] }, ... }`. A toolset is its tools together with its
-- approval policy; one without a policy, with an unknown policy, or with any
-- other key is an error here, as it is when MAG loads the same file.
-- `names` gives, in display order, the MAG expression an author writes for
-- each key.
local function toolsets(opts)
  if opts == nil then return {} end
  local label = "mag-context toolsets"
  if type(opts) ~= "table" then error(label .. " must be a table", 3) end
  local path = nonempty(opts.manifest, label .. ".manifest")
  local ok, manifest = decode_json(read_file(path))
  if not ok then
    error("mag-context: toolsets manifest " .. path .. " is not valid JSON: " .. tostring(manifest), 3)
  end
  if type(manifest) ~= "table" then
    error("mag-context: toolsets manifest " .. path .. " must be a JSON object", 3)
  end
  local out, seen = {}, {}
  for index, entry in ipairs(dense_list(opts.names or {}, label .. ".names")) do
    local here = label .. ".names[" .. index .. "]"
    if type(entry) ~= "table" then error(here .. " must be a table", 3) end
    local key = nonempty(entry.key, here .. ".key")
    local name = nonempty(entry.name, here .. ".name")
    if seen[key] then error(here .. " repeats key " .. key, 3) end
    seen[key] = true
    local where = "mag-context: toolsets manifest " .. path .. " toolset `" .. key .. "`"
    local set = manifest[key]
    if type(set) ~= "table" or #set > 0 then
      error(where .. " must be an object with `policy` and `tools`", 3)
    end
    for field in pairs(set) do
      if field ~= "policy" and field ~= "tools" then
        error(where .. " has unknown key `" .. tostring(field) .. "`; a toolset has exactly `policy` and `tools`", 3)
      end
    end
    if set.policy == nil then
      error(where .. " does not state its approval policy (`policy`: "
        .. table.concat(approval.POLICIES, " or ") .. ")", 3)
    end
    if not approval.is_policy(set.policy) then
      error(where .. " names unknown approval policy `" .. tostring(set.policy) .. "`; expected "
        .. table.concat(approval.POLICIES, " or "), 3)
    end
    local tools = set.tools
    local valid = type(tools) == "table"
    if valid then
      local count = 0
      for _ in pairs(tools) do count = count + 1 end
      for position = 1, count do
        if type(tools[position]) ~= "string" or tools[position] == "" then valid = false end
      end
    end
    if not valid then error(where .. " field `tools` must be an array of tool names", 3) end
    out[index] = { name = name, policy = set.policy, tools = tools }
  end
  if #out == 0 then error(label .. ".names must name at least one toolset", 3) end
  return out
end

local TOOLSETS_INTRO = [[An agent is given one toolset: the tools it can call together with the
approval policy its calls are decided under. It can call only those tools, plus
the runtime's own result tools when its output is a typed value. Each toolset
below is a ready-made `Toolset`; `Toolset {policy: ..., tools: [...]}` builds
another. `ToolApprovalPolicy.ReadOnly` means the agent may only read: a call
that writes or has other effects is rejected in every permission mode.
`ToolApprovalPolicy.Default` is the configured approval behavior for the
current permission mode.]]

local function render_toolsets(sets)
  if #sets == 0 then return nil end
  local lines = { "## Toolsets", "", TOOLSETS_INTRO }
  for _, set in ipairs(sets) do
    lines[#lines + 1] = ""
    lines[#lines + 1] = "`" .. set.name .. "` — policy `ToolApprovalPolicy." .. set.policy .. "`:"
    lines[#lines + 1] = ""
    if #set.tools == 0 then lines[#lines + 1] = "(no tools)" end
    for _, tool in ipairs(set.tools) do lines[#lines + 1] = "- `" .. tool .. "`" end
  end
  return table.concat(lines, "\n")
end

local function append_nonempty(parts, value)
  if type(value) == "string" and value:match("%S") then parts[#parts + 1] = value end
end

local function headed_guide(guide)
  local authored_title = guide.content:match("^%s*#%s+([^\r\n]+)")
  if authored_title == guide.title then return guide.content end
  return "# " .. guide.title .. "\n\n" .. guide.content
end

function Context:compose(base_system, opts)
  opts = opts or {}
  if type(opts) ~= "table" then error("mag-context compose options must be a table", 2) end
  local parts = {}
  append_nonempty(parts, base_system)
  parts[#parts + 1] = REASONER_MENTAL_MODEL

  for _, guide in ipairs(self.guides) do
    parts[#parts + 1] = headed_guide(guide)
  end

  if opts.workspace ~= nil then nonempty(opts.workspace, "mag-context workspace") end

  local references = { "# MAG references\n\nFull MAG Book: `" .. self.book_path ..
    "`\n\nRead its index or the relevant chapter when the quick guides are insufficient." }
  append_nonempty(references, self.public_modules)
  append_nonempty(references, self.toolsets)
  parts[#parts + 1] = table.concat(references, "\n\n")

  if opts.workspace ~= nil then
    parts[#parts + 1] = "# MAG workspace\n\nWritable source directory: `" .. opts.workspace ..
      "`\n\nAdditional source modules may live in this workspace; configured package roots remain immutable."
  end

  for _, section in ipairs(self.trailing_sections) do append_nonempty(parts, section) end
  for _, section in ipairs(dense_list(opts.trailing_sections or {},
      "mag-context compose trailing_sections")) do
    append_nonempty(parts, section)
  end
  return table.concat(parts, "\n\n---\n\n")
end

function M.new(opts)
  if type(opts) ~= "table" then error("mag-context options must be a table", 2) end
  local guides = {}
  for index, guide in ipairs(dense_list(opts.guides or {}, "mag-context guides")) do
    if type(guide) ~= "table" then error("mag-context guide entries must be tables", 2) end
    local path = nonempty(guide.path, "mag-context guides[" .. index .. "].path")
    guides[index] = {
      title = nonempty(guide.title, "mag-context guides[" .. index .. "].title"),
      path = path,
      content = read_file(path),
    }
  end
  if #guides == 0 then error("mag-context requires at least one guide", 2) end

  local trailing = {}
  for index, section in ipairs(dense_list(opts.trailing_sections or {},
      "mag-context trailing_sections")) do
    trailing[index] = nonempty(section, "mag-context trailing_sections[" .. index .. "]")
  end

  local roots = validated_roots(opts.module_roots, "mag-context module_roots")
  return setmetatable({
    guides = guides,
    book_path = nonempty(opts.book_path, "mag-context book_path"),
    public_modules = render_public_modules(public_modules(opts.public_modules, roots)),
    toolsets = render_toolsets(toolsets(opts.toolsets)),
    trailing_sections = trailing,
  }, Context)
end

return M
