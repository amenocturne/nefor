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
        schema = { name = "custom", description = "x", parameters = { type = "object" }, display = { label = "Custom", result = { kind = "content" } } },
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
local projection = assert(display.project({ label = "Read file", primary = { arg = "path" }, result = { kind = "receipt", text = "content loaded" } }, { path = "README.md" }, "payload bytes", false))
assert_true(projection.label == "Read file" and projection.primary == "README.md", "display projects semantic label and primary")
assert_true(projection.result.kind == "receipt" and projection.result.text == "content loaded", "receipt hides successful payload")

print("read-only-tools build_test OK")

-- display contracts are mandatory.
do
  local ok = pcall(rot.build, { extra_tools = { { schema = { name = "missing-display", parameters = { type = "object" } }, handler = function(_, emit) emit.ok("ok") end } } })
  assert_true(not ok, "extra tool without display contract raises")
end

local accepted_displays = {
  { label = "Read", primary = { arg = "path" }, result = { kind = "content" } },
  { label = { arg = "action" }, arguments = { { label = "in", arg = "path" } }, result = { kind = "receipt", text = "done" } },
}
for i, contract in ipairs(accepted_displays) do
  local ok, err = display.validate(contract)
  assert_true(ok, "accepted display " .. i .. ": " .. tostring(err))
end

local rejected_displays = {
  { label = "Read", primary = "literal", result = { kind = "content" } },
  { label = { arg = "" }, result = { kind = "content" } },
  { label = { arg = "path", unknown = true }, result = { kind = "content" } },
  { label = "Read", primary = { arg = 3 }, result = { kind = "content" } },
  { label = "Read", arguments = { { label = "in" } }, result = { kind = "content" } },
  { label = "Read", result = { kind = "content", text = "no" } },
  { label = "Read", result = { kind = "receipt" } },
  { label = "Read", result = { kind = "other" } },
}
for i, contract in ipairs(rejected_displays) do
  local ok = display.validate(contract)
  assert_true(not ok, "rejected display " .. i .. " unexpectedly accepted")
end

-- The same JSON corpus drives Rust and Lua validators. Decoding is
-- essential here: it preserves exact [] versus {} identity at the wire boundary.
do
  local root = os.getenv("NEFOR_REPO_ROOT") or "."
  local fh = assert(io.open(root .. "/tests/fixtures/tool_display_contracts.json", "r"))
  local fixtures = nefor.json.decode(fh:read("*a")); fh:close()
  for _, fixture in ipairs(fixtures) do
    local ok = display.validate(fixture.contract)
    assert_true((ok == true) == fixture.valid, "shared display fixture: " .. fixture.name)
  end
end

-- Config-owned non-empty dense lists remain valid, while malformed Lua
-- maps and sparse/non-list tables cannot masquerade as JSON arrays.
do
  local ok = display.validate({ label = "Read", arguments = { [1] = { label = "in", arg = "path" }, [3] = { label = "out", arg = "dest" } }, result = { kind = "content" } })
  assert_true(not ok, "sparse Lua arguments rejected")
  ok = display.validate({ label = "Read", arguments = { named = { label = "in", arg = "path" } }, result = { kind = "content" } })
  assert_true(not ok, "non-list Lua arguments rejected")
end
