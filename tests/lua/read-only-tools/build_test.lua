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
assert_true(projection.result.kind == "receipt" and projection.result.text:find("content loaded", 1, true), "receipt hides successful payload")
assert_true(not projection.result.text:find("payload bytes", 1, true), "receipt does not copy the successful payload")

-- Projection is a read-only view: exact invocation and result identities survive.
do
  local names = { "dev-mode", "workspace-routing" }
  local args = { name = names, nested = { evidence = "untouched" } }
  local output = "exact model-facing bytes"
  local args_identity, names_identity = args, names
  local projected = assert(display.project(
    { label = "Load instructions", primary = { arg = "name" }, result = { kind = "receipt", text = "instructions loaded" } },
    args, output, false, "instructions"))
  assert_true(projected.primary == "dev-mode · workspace-routing", "arrays retain every exact instruction name")
  assert_true(args == args_identity and args.name == names_identity, "projection preserves invocation table identity")
  assert_true(args.nested.evidence == "untouched" and output == "exact model-facing bytes", "projection does not mutate nested args or result bytes")
end

-- Unknown tools retain tool identity, field names, and structural evidence only.
-- Invocation scalar values are raw-only regardless of their key names.
do
  local secret_fields = {
    action = "SECRET ACTION",
    branch = "SECRET BRANCH",
    command = "SECRET COMMAND",
    file = "SECRET FILE",
    intent = "SECRET INTENT",
    name = "SECRET NAME",
    path = "SECRET PATH",
    query = "SECRET QUERY",
    repository = "SECRET REPOSITORY",
    run_id = "SECRET RUN ID",
    scope = "SECRET SCOPE",
    target = "SECRET TARGET",
    task = "SECRET TASK",
    view = "SECRET VIEW",
    token = "SECRET TOKEN",
    password = "SECRET PASSWORD",
    api_key = "SECRET API KEY",
    body = "SECRET BODY",
    arbitrary = "SECRET ARBITRARY",
  }
  local args = {}
  for field, secret in pairs(secret_fields) do args[field] = secret end
  args.payload = { secret = "SECRET PAYLOAD" }
  args.items = { "SECRET ARRAY ITEM", "SECOND SECRET" }
  args.zeta = 8675309
  args.enabled = true

  local projected = assert(display.project(nil, args, "SECRET OUTPUT", false, "mystery_tool"))
  assert_true(projected.label == "mystery_tool" and projected.primary == nil, "unknown fallback preserves only tool identity")
  local rendered = nefor.json.encode(projected)
  for field, secret in pairs(secret_fields) do
    assert_true(not rendered:find(secret, 1, true), "unknown fallback does not expose " .. secret)
    assert_true(rendered:find(field, 1, true), "unknown fallback retains field name " .. field)
  end
  for _, secret in ipairs({ "SECRET PAYLOAD", "SECRET ARRAY ITEM", "SECOND SECRET", "8675309" }) do
    assert_true(not rendered:find(secret, 1, true), "unknown fallback does not expose " .. secret)
  end
  assert_true(rendered:find("string", 1, true) and rendered:find("object", 1, true)
    and rendered:find("number", 1, true) and rendered:find("boolean", 1, true)
    and rendered:find("array", 1, true) and rendered:find("2 items", 1, true)
    and rendered:find(" B", 1, true), "unknown fallback retains types, counts, and byte sizes")
  assert_true(not projected.result.text:find("SECRET OUTPUT", 1, true), "unknown fallback hides successful output")

  local scalar_cases = {
    { value = "SECRET SCALAR", secret = "SECRET SCALAR", kind = "string" },
    { value = 123456789, secret = "123456789", kind = "number" },
    { value = true, secret = "true", kind = "boolean" },
  }
  for _, case in ipairs(scalar_cases) do
    local scalar_projection = assert(display.project(nil, case.value, "ok", false, "scalar_tool"))
    local scalar_rendered = nefor.json.encode(scalar_projection)
    assert_true(not scalar_rendered:find(case.secret, 1, true), "non-table fallback does not expose scalar input")
    assert_true(scalar_rendered:find("input", 1, true) and scalar_rendered:find(case.kind, 1, true)
      and scalar_rendered:find(" B", 1, true), "non-table fallback retains input type and byte size")
  end
  local array_projection = assert(display.project(nil, { "TOP LEVEL ARRAY SECRET", false }, "ok", false, "array_tool"))
  local array_rendered = nefor.json.encode(array_projection)
  assert_true(not array_rendered:find("TOP LEVEL ARRAY SECRET", 1, true) and not array_rendered:find("false", 1, true), "array input values remain raw-only")
  assert_true(array_rendered:find("array", 1, true) and array_rendered:find("2 items", 1, true)
    and array_rendered:find(" B", 1, true), "array input retains type, count, and byte size")

  local failed = assert(display.project(nil, args, "exact failure diagnostic", true, "mystery_tool"))
  assert_true(failed.result.error and failed.result.text == "exact failure diagnostic", "unknown failure remains visible")
end

-- Every starter registry tool projects table-driven without exposing a
-- successful payload. Skill is included as the config-selectable array case.
do
  local cases = {
    { "read_file", { label = "Read file", primary = { arg = "path", cwd_arg = "cwd" }, arguments = { { label = "offset", arg = "offset" }, { label = "max bytes", arg = "max_bytes" } }, result = { kind = "receipt", text = "content loaded" } }, { path = "src/main.rs", cwd = "/repo-a", offset = 4, max_bytes = 20 }, "src/main.rs (cwd: /repo-a)" },
    { "read_image", { label = "Read image", primary = { arg = "path", cwd_arg = "cwd" }, result = { kind = "receipt", text = "image loaded" } }, { path = "shot.png", cwd = "/repo-b" }, "shot.png (cwd: /repo-b)" },
    { "write_file", { label = "Write file", primary = { arg = "path", cwd_arg = "cwd" }, result = { kind = "receipt", text = "file written" } }, { path = "out.txt", cwd = "/repo-c", content = "PRIVATE" }, "out.txt (cwd: /repo-c)" },
    { "edit_file", { label = "Edit file", primary = { arg = "path", cwd_arg = "cwd" }, result = { kind = "receipt", text = "file edited" } }, { path = "out.txt", cwd = "/repo-d", old_string = "PRIVATE", new_string = "CHANGED" }, "out.txt (cwd: /repo-d)" },
    { "shell.script", { label = "Run command", primary = { arg = "command" }, arguments = { { label = "in", arg = "cwd" }, { label = "timeout ms", arg = "timeout_ms" } }, result = { kind = "content" } }, { command = "rg TODO", cwd = "repo", stdin = "PRIVATE" }, "rg TODO" },
    { "search_text", { label = "Search text", primary = { arg = "pattern" }, arguments = { { label = "in", arg = "path", cwd_arg = "cwd", default = "." }, { label = "type", arg = "file_type" }, { label = "glob", arg = "glob" }, { label = "limit", arg = "max_results" } }, result = { kind = "content" } }, { pattern = "TODO", path = "src", cwd = "/repo-e" }, "TODO", "src (cwd: /repo-e)" },
    { "list_dir", { label = "List directory", primary = { arg = "path" }, result = { kind = "content" } }, { path = "src" }, "src" },
    { "python-read", { label = "Analyze workspace", primary = { arg = "task" }, result = { kind = "content" } }, { task = "Count modules" }, "Count modules" },
    { "instructions", { label = "Load instructions", primary = { arg = "name" }, result = { kind = "receipt", text = "instructions loaded" } }, { name = { "dev-mode", "workspace-routing" } }, "dev-mode · workspace-routing" },
    { "discover_instruction_files", { label = "Discover instructions", primary = { arg = "path" }, arguments = { { label = "scope", arg = "scope" }, { label = "unread only", arg = "unread_only" } }, result = { kind = "content" } }, { path = "repo", scope = "auto", unread_only = true }, "repo" },
    { "skill", { label = "Load skill", primary = { arg = "name" }, result = { kind = "receipt", text = "skill loaded" } }, { name = { "html-report", "browser-audit" } }, "html-report · browser-audit" },
    { "graph-status", { label = "Graph status", primary = { arg = "run_id" }, result = { kind = "content" } }, { run_id = "run-1" }, "run-1" },
    { "await-run", { label = "Await run", primary = { arg = "run_id" }, result = { kind = "content" } }, { run_id = "run-1" }, "run-1" },
    { "terminate-graph", { label = "Terminate graph", primary = { arg = "run_id" }, result = { kind = "content" } }, { run_id = "run-1" }, "run-1" },
    { "write-review", { label = "Review plan", primary = { arg = "view" }, result = { kind = "content" } }, { view = "inline", plan = "PRIVATE" }, "inline" },
    { "mag", { label = "MAG", primary = { arg = "file" }, arguments = { { label = "action", arg = "action" } }, result = { kind = "content" } }, { file = "build.mag", action = "execute", content = "PRIVATE" }, "build.mag" },
    { "mag-eval", { label = "mag-eval", primary = { arg = "intent" }, result = { kind = "content" } }, { intent = "Inspect registry", expression = "PRIVATE" }, "Inspect registry" },
    { "git_worktree_create", { label = "Create worktree", primary = { arg = "path" }, arguments = { { label = "branch", arg = "branch" }, { label = "base", arg = "base" } }, result = { kind = "receipt", text = "worktree created" } }, { path = "/tmp/wt", branch = "topic", base = "main" }, "/tmp/wt" },
    { "git_worktree_open", { label = "Open worktree", primary = { arg = "path" }, arguments = { { label = "branch", arg = "branch" }, { label = "repository", arg = "repository" } }, result = { kind = "receipt", text = "worktree opened" } }, { path = "/tmp/wt", branch = "topic", repository = "/repo" }, "/tmp/wt" },
  }
  for _, case in ipairs(cases) do
    local projected, err = display.project(case[2], case[3], "SECRET SUCCESS " .. case[1], false, case[1])
    assert_true(projected ~= nil, case[1] .. " projects: " .. tostring(err))
    assert_true(projected.primary == case[4], case[1] .. " keeps exact target/action")
    if case[5] then
      local found = false
      for _, argument in ipairs(projected.arguments) do
        if argument.value == case[5] then found = true end
      end
      assert_true(found, case[1] .. " keeps exact cwd-sensitive target")
    end
    assert_true(not projected.result.text:find("SECRET SUCCESS", 1, true), case[1] .. " hides successful body")
  end

  local absolute = assert(display.project(cases[1][2], { path = "/absolute/file", cwd = "/ignored" }, "ok", false, "read_file"))
  assert_true(absolute.primary == "/absolute/file", "absolute targets do not misleadingly include cwd")
  local default_search = assert(display.project(cases[6][2], { pattern = "TODO", cwd = "/repo-f" }, "ok", false, "search_text"))
  assert_true(default_search.arguments[1].value == ". (cwd: /repo-f)", "default search path remains cwd-unambiguous")
end

-- display contracts are mandatory.
do
  local ok = pcall(rot.build, { extra_tools = { { schema = { name = "missing-display", parameters = { type = "object" } }, handler = function(_, emit) emit.ok("ok") end } } })
  assert_true(not ok, "extra tool without display contract raises")
end

local accepted_displays = {
  { label = "Read", primary = { arg = "path" }, result = { kind = "content" } },
  { label = "Read", primary = { arg = "path", cwd_arg = "cwd" }, result = { kind = "content" } },
  { label = { arg = "action" }, arguments = { { label = "in", arg = "path", cwd_arg = "cwd", default = "." } }, result = { kind = "receipt", text = "done" } },
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
  { label = "Read", primary = { arg = "path", cwd_arg = "" }, result = { kind = "content" } },
  { label = "Read", primary = { arg = "path", default = 42 }, result = { kind = "content" } },
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
