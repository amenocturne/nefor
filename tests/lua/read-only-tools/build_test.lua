-- tests/lua/read-only-tools/build_test.lua — the opt-in `include` seam.
--
-- Base tools are advertised only when a config names them in `build{ include }`.
-- Driven by engine/tests/read_only_tools_test.rs (stubs nefor json/log, wires
-- package.path). This file stubs nefor.engine.send to capture the advertise.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local captured
nefor.engine = {
  now  = function() return 0 end,
  send = function(payload)
    local ok, decoded = pcall(nefor.json.decode, payload)
    if ok and type(decoded) == "table" and type(decoded.body) == "table"
       and decoded.body.kind == "tool-gate.tools.advertise" then
      captured = decoded.body.tools
    end
  end,
}

NEFOR_CONFIG_DIR = "/custom/runtime-config"
NEFOR_DATA_DIR = "/custom/runtime-data"

local rot = require("libs.read-only-tools")

local function advertised_tools(opts)
  captured = nil
  local spec = rot.build(opts)
  local hello = nefor.json.encode({ body = { kind = "tool-gate.hello" } })
  spec.receive_msg({ origin = "plugin", payload = hello })
  return captured or {}
end

-- Build a spec, drive a tool-gate.hello, return the advertised names as a set.
local function advertised_names(opts)
  local names = {}
  for _, t in ipairs(advertised_tools(opts)) do names[t.name] = true end
  return names
end

-- opt-in: only the included base tools are advertised.
do
  local n = advertised_names { include = { "list_dir", "skill" } }
  assert_true(n.list_dir, "list_dir advertised when included")
  assert_true(n.skill, "skill advertised when included")
  assert_true(not n.search_text, "search_text NOT advertised when omitted")
  assert_true(not n["python-read"], "python-read NOT advertised when omitted")
  assert_true(not n.instructions, "instructions NOT advertised when omitted")
  assert_true(not n.discover_instruction_files, "discover NOT advertised when omitted")
end

-- skill description exposes the effective config-root convention.
do
  local skill
  for _, tool in ipairs(advertised_tools { include = { "skill" } }) do
    if tool.name == "skill" then skill = tool end
  end
  assert_true(skill ~= nil, "skill schema advertised")
  assert_true(
    skill.description:find("/custom/runtime-config/skills/<name>/skill.md", 1, true),
    "skill description contains resolved config-root path convention")
  assert_true(
    not skill.description:find("/custom/runtime-data", 1, true),
    "skill description does not use data root")
  assert_true(
    not skill.description:find("<config>", 1, true),
    "skill description does not leave config root ambiguous")
end

-- omitted include => no base tools (pure opt-in default; no contamination).
do
  local n = advertised_names {}
  assert_true(next(n) == nil, "no base tools advertised without include")
end

-- extra_tools ride alongside the included base tools.
do
  local n = advertised_names {
    include = { "list_dir" },
    extra_tools = {
      {
        schema = { name = "custom", description = "x", parameters = { type = "object" }, display = {
          compact = { label = "Custom" }, expanded = { label = "Custom", fields = {} },
          result = { kind = "content", fields = {} },
        } },
        handler = function(_, emit) emit.ok("ok") end,
      },
    },
  }
  assert_true(n.list_dir, "included base advertised alongside extra")
  assert_true(n.custom, "extra tool advertised")
end

-- unknown base tool name in include errors (catches typos loudly).
do
  local ok = pcall(rot.build, { include = { "does_not_exist" } })
  assert_true(not ok, "unknown base tool in include raises")
end

-- duplicate base tool in include errors.
do
  local ok = pcall(rot.build, { include = { "list_dir", "list_dir" } })
  assert_true(not ok, "duplicate base tool in include raises")
end

-- a base tool colliding with an extra tool name errors.
do
  local ok = pcall(rot.build, {
    include = { "list_dir" },
    extra_tools = {
      {
        schema = { name = "list_dir", description = "x", parameters = { type = "object" } },
        handler = function(_, emit) emit.ok("ok") end,
      },
    },
  })
  assert_true(not ok, "extra tool colliding with an included base name raises")
end

local display = require("libs.chat.tool_display")
local contract = {
  compact = { label = "Read file", primary = { label = "path", select = { source = "args", path = "path" }, kind = "path" } },
  expanded = { label = "Read file", fields = {} },
  result = { kind = "receipt", text = "content loaded", fields = {} },
}
local projection = assert(display.project(contract, { path = "README.md" }, "payload bytes", false))
assert_true(projection.label == "Read file" and projection.primary == "README.md", "display projects semantic label and primary")

return true
