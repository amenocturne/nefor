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

local rot = require("libs.read-only-tools")

-- Build a spec, drive a tool-gate.hello, return the advertised names as a set.
local function advertised_names(opts)
  captured = nil
  local spec = rot.build(opts)
  local hello = nefor.json.encode({ body = { kind = "tool-gate.hello" } })
  spec.receive_msg({ origin = "plugin", payload = hello })
  local names = {}
  for _, t in ipairs(captured or {}) do names[t.name] = true end
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
        schema = { name = "custom", description = "x", parameters = { type = "object" } },
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

print("read-only-tools build_test OK")
