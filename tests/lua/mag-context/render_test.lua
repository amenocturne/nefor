-- Rendering of public MAG modules and toolsets by libs.mag-context.
-- Driven by engine/tests/mag_context_test.rs, which supplies `_scratch`.

local mag_context = require("libs.mag-context")

local function write(relative, content)
  local path = _scratch .. "/" .. relative
  os.execute('mkdir -p "' .. path:match("^(.*)/[^/]+$") .. '"')
  local file = assert(io.open(path, "wb"))
  file:write(content)
  file:close()
  return path
end

local function expect_error(fragment, build)
  local ok, err = pcall(build)
  assert(not ok, "expected an error containing: " .. fragment)
  assert(tostring(err):find(fragment, 1, true),
    "error `" .. tostring(err) .. "` does not contain `" .. fragment .. "`")
end

local AGENTS = "// Agents of this configuration.\nlet standard = 1\n"
local FACADE = "// no trailing newline, with a ``` fence inside\nlet node = 2"
local INTERNAL = "let hidden = 3\n"

local guide = write("guide.md", "# Guide\n\nGuide body.\n")
local core_root = _scratch .. "/core/lib"
local config_root = _scratch .. "/config/mag/lib"
local agents = write("config/mag/lib/agents.mag", AGENTS)
local facade = write("core/lib/nefor.mag", FACADE)
local nested = write("core/lib/nefor/extra/kit.magl", "(kit)\n")
write("core/lib/nefor/graph.mag", INTERNAL)
local manifest = write("config/mag/lib/nefor/toolsets.json",
  '{"general": {"policy": "Default", "tools": ["read_file", "write_file", "mag-apply"]},'
  .. ' "read_only": {"policy": "ReadOnly", "tools": ["read_file"]},'
  .. ' "none": {"policy": "ReadOnly", "tools": []},'
  .. ' "bare": ["read_file"], "no_policy": {"tools": ["read_file"]},'
  .. ' "odd_policy": {"policy": "Yolo", "tools": []},'
  .. ' "extra": {"policy": "Default", "tools": [], "gate": "auto"},'
  .. ' "broken": {"policy": "Default", "tools": {"read_file": true}},'
  .. ' "mixed": {"policy": "Default", "tools": ["read_file", 7]}}')

local function context(overrides)
  local opts = {
    guides = { { title = "Guide", path = guide } },
    book_path = "/book/README.md",
    module_roots = {
      { name = "nefor-mag", path = core_root },
      { name = "config", path = config_root .. "/" },
    },
  }
  for key, value in pairs(overrides or {}) do opts[key] = value end
  return mag_context.new(opts)
end

-- No public modules and no toolsets: the references name the book only.
do
  local text = context():compose("BASE", { workspace = "/session/mag" })
  assert(text:find("Full MAG Book: `/book/README.md`", 1, true))
  assert(not text:find("Public modules", 1, true), "nothing is advertised by default")
  assert(not text:find("Toolsets", 1, true))
  assert(not text:find("nefor.graph", 1, true), "modules under the roots are not inventoried")
  assert(text:find("Writable source directory: `/session/mag`", 1, true))
end

-- Order, names, paths, verbatim source, and embedded versus listed-only.
do
  local text = context({
    public_modules = { agents, { path = facade, embed = false }, { path = nested } },
    toolsets = {
      manifest = manifest,
      names = {
        { key = "read_only", name = "nefor.read_only_tools" },
        { key = "general", name = "tools.general_tools" },
        { key = "none", name = "tools.no_tools" },
      },
    },
    trailing_sections = { "TRAILING" },
  }):compose("BASE", { workspace = "/session/mag" })

  local function at(fragment)
    return assert(text:find(fragment, 1, true), "missing: " .. fragment)
  end
  local base = at("BASE")
  local guide_at = at("# Guide")
  local references = at("# MAG references")
  local list_agents = at("- `agents` — `" .. agents .. "` (source below)")
  local list_facade = at("- `nefor` — `" .. facade .. "` (read when needed)")
  local list_nested = at("- `nefor.extra.kit` — `" .. nested .. "` (source below)")
  local source_agents = at("### Module `agents` — `" .. agents .. "`\n\n```mag\n" .. AGENTS .. "```")
  local source_nested = at("### Module `nefor.extra.kit` — `" .. nested .. "`\n\n```mag\n(kit)\n```")
  local toolsets = at("## Toolsets")
  local workspace = at("# MAG workspace")
  local trailing = at("TRAILING")
  assert(base < guide_at and guide_at < references and references < list_agents
    and list_agents < list_facade and list_facade < list_nested
    and list_nested < source_agents and source_agents < source_nested
    and source_nested < toolsets and toolsets < workspace and workspace < trailing,
    "sections keep their order")
  assert(not text:find("### Module `nefor` ", 1, true), "a listed-only module is not embedded")
  assert(not text:find("let node = 2", 1, true))
  assert(not text:find("nefor.graph", 1, true), "an unlisted module is not advertised")
  assert(not text:find("config:", 1, true), "root names are not rendered as headings")

  -- Each toolset is shown with its approval policy next to its tools.
  at("`nefor.read_only_tools` — policy `ToolApprovalPolicy.ReadOnly`:\n\n- `read_file`\n\n"
    .. "`tools.general_tools` — policy `ToolApprovalPolicy.Default`:\n\n"
    .. "- `read_file`\n- `write_file`\n- `mag-apply`\n\n"
    .. "`tools.no_tools` — policy `ToolApprovalPolicy.ReadOnly`:\n\n(no tools)")
  at("An agent is given one toolset: the tools it can call together with the\napproval policy")
end

-- A source containing a fence is wrapped in a longer one and stays verbatim.
do
  local text = context({ public_modules = { facade } }):compose("")
  assert(text:find("````mag\n" .. FACADE .. "\n````", 1, true), "longer fence, bytes unchanged")
end

expect_error("is inside none", function()
  context({ public_modules = { write("elsewhere/agents.mag", AGENTS) } })
end)
expect_error("cannot read", function()
  context({ public_modules = { config_root .. "/missing.mag" } })
end)
expect_error("cannot read", function()
  context({ public_modules = { { path = config_root .. "/missing.mag", embed = false } } })
end)
expect_error("is not a .mag or .magl source", function()
  context({ public_modules = { manifest } })
end)
expect_error("repeats module agents", function()
  context({ public_modules = { agents, { path = agents, embed = false } } })
end)
expect_error("unknown field", function()
  context({ public_modules = { { path = agents, embedded = false } } })
end)
expect_error("is inside several", function()
  context({
    module_roots = { { name = "a", path = core_root }, { name = "b", path = core_root .. "/nefor" } },
    public_modules = { nested },
  })
end)

expect_error("is not valid JSON", function()
  context({ toolsets = { manifest = write("bad.json", "{ nope"),
    names = { { key = "general", name = "g" } } } })
end)
expect_error("cannot read", function()
  context({ toolsets = { manifest = _scratch .. "/absent.json",
    names = { { key = "general", name = "g" } } } })
end)
-- A toolset states its policy and its tools and nothing else; anything short
-- of that is an error when the prompt context is built.
for key, message in pairs({
  absent = "toolset `absent` must be an object with `policy` and `tools`",
  bare = "toolset `bare` must be an object with `policy` and `tools`",
  no_policy = "toolset `no_policy` does not state its approval policy (`policy`: ReadOnly or Default)",
  odd_policy = "toolset `odd_policy` names unknown approval policy `Yolo`; expected ReadOnly or Default",
  extra = "toolset `extra` has unknown key `gate`; a toolset has exactly `policy` and `tools`",
  broken = "toolset `broken` field `tools` must be an array of tool names",
  mixed = "toolset `mixed` field `tools` must be an array of tool names",
}) do
  expect_error(message, function()
    context({ toolsets = { manifest = manifest, names = { { key = key, name = "g" } } } })
  end)
end
expect_error("must name at least one toolset", function()
  context({ toolsets = { manifest = manifest, names = {} } })
end)
