-- init.lua — starter composition.
--
-- Lua libraries are bootstrapped via sparse-clone of the upstream repo
-- into ~/.local/share/nefor/nefor/ (same as end-user install).
--
-- Bootstrap order:
--   1. NEFOR_DEV_DIR set     → in-checkout dev mode; resolve from there.
--   2. NEFOR_LOCAL_DIR set   → local checkout override for installed configs.
--   3. agentic-kit.json nefor_repo → per-install local checkout override.
--   4. sibling checkout      → auto-detected from agentic-kit.json dev workspace.
--   5. STARTER_ROOT/../lua/nefor-pm exists → in-checkout (running via
--                                            `just run`); use ../lua etc.
--   6. otherwise             → sparse-clone amenocturne/nefor at the
--                              pinned ref into <DATA>/nefor/, use that.

local STARTER_ROOT = NEFOR_CONFIG_DIR or "."
local NEFOR_DEV_DIR = os.getenv("NEFOR_DEV_DIR")
local NEFOR_LOCAL_DIR = os.getenv("NEFOR_LOCAL_DIR")

local UPSTREAM_REF
do
  local v = nefor and nefor.version
  if type(v) == "string" and v:match("^%d+%.%d+%.%d+$") then
    UPSTREAM_REF = "v" .. v
  else
    UPSTREAM_REF = "main"
  end
end
local SPARSE_CONE  = "lua starter plugins"

local function path_exists(p)
  if nefor and nefor.fs and nefor.fs.exists then
    return nefor.fs.exists(p)
  end
  local f = io.open(p, "r")
  if f then f:close(); return true end
  return false
end

local function run(cmd)
  local ok = os.execute(cmd)
  return ok == true or ok == 0
end

local function sh_quote(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function valid_nefor_root(root)
  return root and #root > 0 and path_exists(root .. "/lua/nefor-pm/init.lua")
end

local function explicit_nefor_root(env_name, root)
  if not root or #root == 0 then return nil end
  if valid_nefor_root(root) then return root end
  error("nefor bootstrap: " .. env_name .. "=" .. root
        .. " does not contain lua/nefor-pm/init.lua")
end

local function read_agentic_kit_path(key)
  local fh = io.open(STARTER_ROOT .. "/agentic-kit.json", "r")
  if not fh then return nil end
  local raw = fh:read("*a")
  fh:close()
  local ok, decoded = pcall(nefor.json.decode, raw)
  if ok and type(decoded) == "table" and type(decoded[key]) == "string" then
    return decoded[key]
  end
  return raw:match('"' .. key .. '"%s*:%s*"([^"]+)"')
end

local function read_agentic_kit_nefor_repo()
  return read_agentic_kit_path("nefor_repo")
end

local function read_agentic_kit_dev_workspace()
  return read_agentic_kit_path("dev_workspace")
end

local function detected_local_nefor_root()
  local dev_workspace = read_agentic_kit_dev_workspace()
  if dev_workspace then
    local candidate = dev_workspace .. "/personal/nefor"
    if valid_nefor_root(candidate) then return candidate end
  end
  local sibling = STARTER_ROOT .. "/../../../../nefor"
  if valid_nefor_root(sibling) then return sibling end
  return nil
end

local function ensure_upstream_checkout(pm_root)
  local root = sh_quote(pm_root)
  local ref = sh_quote(UPSTREAM_REF)
  local fetch_ref = ref
  if UPSTREAM_REF:match("^v%d+%.%d+%.%d+$") then
    fetch_ref = "tag " .. ref
  end

  if not path_exists(pm_root) then
    nefor.fs.mkdir_p(nefor.fs.data_root())
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "--branch " .. ref .. " "
                   .. "https://github.com/amenocturne/nefor.git " .. root
    if not run(clone_cmd) then
      error("nefor bootstrap: git clone failed for ref " .. UPSTREAM_REF
            .. ". Check git is on PATH, the network is reachable, and the "
            .. "ref exists on origin.")
    end
  elseif not path_exists(pm_root .. "/.git") then
    error("nefor bootstrap: " .. pm_root .. " exists but is not a git checkout")
  else
    if not run("git -C " .. root .. " fetch --depth 1 origin " .. fetch_ref) then
      error("nefor bootstrap: git fetch failed for ref " .. UPSTREAM_REF)
    end
    -- This tree is a managed cache; keep Lua assets matched to the binary version.
    if not run("git -C " .. root .. " checkout --force FETCH_HEAD") then
      error("nefor bootstrap: git checkout failed for ref " .. UPSTREAM_REF)
    end
  end

  if not run("git -C " .. root .. " sparse-checkout set " .. SPARSE_CONE) then
    error("nefor bootstrap: git sparse-checkout failed for " .. pm_root)
  end
end

local NEFOR_ROOT
local explicit_root = explicit_nefor_root("NEFOR_DEV_DIR", NEFOR_DEV_DIR)
                   or explicit_nefor_root("NEFOR_LOCAL_DIR", NEFOR_LOCAL_DIR)
                   or explicit_nefor_root(
                     "agentic-kit.json nefor_repo",
                     read_agentic_kit_nefor_repo())
local detected_root = detected_local_nefor_root()
if explicit_root then
  NEFOR_ROOT = explicit_root
elseif detected_root then
  NEFOR_ROOT = detected_root
elseif path_exists(STARTER_ROOT .. "/../lua/nefor-pm/init.lua") then
  NEFOR_ROOT = STARTER_ROOT .. "/.."
else
  local data_dir = nefor.fs.data_root()
  local pm_root  = data_dir .. "/nefor"
  ensure_upstream_checkout(pm_root)
  NEFOR_ROOT = pm_root
end

local LUA_ROOT = NEFOR_ROOT .. "/lua"

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm wires the core primitives, generic libs, and every plugin
-- lib. Every entry's `dir` resolves from NEFOR_ROOT (whichever way
-- the bootstrap above picked it). `tag` matches UPSTREAM_REF so a
-- future pm consistency check or refresh path uses one source of truth.
local pm = require("nefor-pm")
pm.install({
  {
    "amenocturne/nefor",
    name = "core",
    tag  = UPSTREAM_REF,
    path = "lua/core/",
    dir  = NEFOR_ROOT .. "/lua/core",
  },

  {
    "amenocturne/nefor",
    name = "libs",
    tag  = UPSTREAM_REF,
    path = "lua/libs/",
    dir  = NEFOR_ROOT .. "/lua/libs",
  },

  {
    "amenocturne/nefor",
    name = "openai-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/openai-provider/lua/openai-provider/",
    dir  = NEFOR_ROOT .. "/plugins/openai-provider/lua/openai-provider",
  },

  {
    "amenocturne/nefor",
    name = "chatgpt-provider",
    tag  = UPSTREAM_REF,
    path = "plugins/chatgpt-provider/lua/chatgpt-provider/",
    dir  = NEFOR_ROOT .. "/plugins/chatgpt-provider/lua/chatgpt-provider",
  },

  {
    "amenocturne/nefor",
    name = "tool-gate",
    tag  = UPSTREAM_REF,
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = NEFOR_ROOT .. "/plugins/tool-gate/lua/tool-gate",
  },

  {
    "amenocturne/nefor",
    name = "nefor-tui",
    tag  = UPSTREAM_REF,
    path = "plugins/nefor-tui/lua/",
    dir  = NEFOR_ROOT .. "/plugins/nefor-tui/lua",
  },

  {
    "amenocturne/nefor",
    name = "reasoner-graph",
    tag  = UPSTREAM_REF,
    path = "plugins/reasoner-graph/lua/reasoner-graph/",
    dir  = NEFOR_ROOT .. "/plugins/reasoner-graph/lua/reasoner-graph",
  },
})

local ncp            = require("core.ncp")
local actor          = require("core.actor")
local history_replay = require("core.history_replay")
local sessions       = require("sessions")
local cfg            = require("config").active
local lead_role      = require("lead-workflow.role")

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()
require("core.combinator_shim").install()
-- Defense-in-depth fallback for the synchronous `history_replay.set`
-- path that sessions drives around its replay burst. Wired explicitly
-- here so module load stays free of bus dependencies.
history_replay.install()
actor.spawn(sessions)
actor.spawn(require("state-tracking"))

local function parse_startup_args(argv)
  local opts = { session_id = nil, prompt = nil }
  local i = 1
  while i <= #argv do
    local a = argv[i]
    if a == "--session" then
      local v = argv[i + 1]
      if type(v) ~= "string" or v == "" then
        error("--session requires a session id")
      end
      opts.session_id = v
      i = i + 2
    elseif a == "--prompt" then
      local v = argv[i + 1]
      if type(v) ~= "string" or v == "" then
        error("--prompt requires a prompt")
      end
      opts.prompt = v
      i = i + 2
    else
      error("unknown startup arg: " .. tostring(a))
    end
  end
  return opts
end

local startup = parse_startup_args((nefor.runtime and nefor.runtime.argv) or {})
sessions.init(startup.session_id)

-- Spawn order matters: type-tag registrations must complete before the
-- scheduler queries on submit. Order:
--   1. libs.generic-{provider,tool}.declare()
--   2. agentic-loop + reasoners
--   3. providers
--   4. reasoner-graph + tool-gate + basic-tools
--   5. lead-workflow
--   6. chat (declarative TUI)

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

-- The actor runtime queues incoming envelopes during boot, so spawning
-- the orchestrator and its resident reasoners before the plugins they
-- coordinate is safe even if a plugin's `ready` arrives early.
-- Build runtime context: cwd, workspace index, agentic-kit paths.
-- Appended to the lead system prompt so the agent knows where it's
-- operating and what projects are available.
local function build_runtime_context()
  local parts = {}

  local cwd = nefor and nefor.fs and nefor.fs.cwd and nefor.fs.cwd()
  if not cwd then
    local p = io.popen("pwd")
    if p then cwd = p:read("*l"); p:close() end
  end
  if cwd then
    parts[#parts + 1] = "## Working directory\n\n`" .. cwd .. "`"
  end

  if #parts == 0 then return "" end
  return "\n\n---\n\n# Runtime Context\n\n" .. table.concat(parts, "\n\n")
end

local agentic_loop = require("agentic-loop")
agentic_loop.configure {
  provider         = cfg.default_provider,
  model            = cfg.default_model,
  reasoning_effort = cfg.lead_reasoning_effort,
  system           = lead_role.LEAD_SYSTEM_PROMPT .. build_runtime_context(),
  -- Restrict the lead's chat catalog to the orchestration-tool surface.
  -- Without this filter the lead sees every wire-advertised tool — most
  -- problematically `spawn_graph` (the reasoner-graph internal that
  -- `mag` translates into) — and could call them directly,
  -- bypassing the role-keyed sub-agent contract and bottoming out in
  -- `reasoner '<role>' not connected` runtime errors. The agent
  -- reasoner already enforces a per-role allowlist on its sub-firings
  -- via the same `chat.create.tools` plumbing; this extends the same
  -- discipline to the lead's chat at the orchestrator layer.
  tool_allowlist = lead_role.ORCHESTRATION_TOOLS,
}
actor.spawn(agentic_loop)
actor.spawn(require("reasoners"))

local provider = require("compositors.provider")
for _, p in ipairs(cfg.providers or {}) do
  if p.kind == "mock" then
    -- mock-plugin speaks the same wire protocol as the openai-provider
    -- binary, so the same actor spec works — only the binary differs.
    actor.spawn(provider.spawn_spec(
      p.name,
      {
        require("config").bin("mock-plugin"),
        "--script", STARTER_ROOT .. "/" .. p.mock_script,
      },
      { agentic_loop = agentic_loop }
    ))
  elseif p.kind == "openai" then
    local provider_command = {
      require("config").bin("openai-provider"),
      "--name",     p.name,
      "--base-url", p.base_url,
    }
    if p.model then
      table.insert(provider_command, "--model")
      table.insert(provider_command, p.model)
    end
    for _, a in ipairs(p.extra_args or {}) do
      table.insert(provider_command, a)
    end
    actor.spawn(provider.spawn_spec(
      p.name,
      provider_command,
      { static_token = p.static_token, agentic_loop = agentic_loop }
    ))
  elseif p.kind == "chatgpt" then
    local provider_command = {
      require("config").bin("chatgpt-provider"),
      "--name", p.name,
    }
    if p.base_url then
      table.insert(provider_command, "--base-url")
      table.insert(provider_command, p.base_url)
    end
    -- No `--model` flag: chatgpt-provider fetches its model list from
    -- the backend at runtime; the user picks via `/model` in chat.
    for _, a in ipairs(p.extra_args or {}) do
      table.insert(provider_command, a)
    end
    actor.spawn(provider.spawn_spec(
      p.name,
      provider_command,
      { translator_lib = "chatgpt-provider", agentic_loop = agentic_loop }
    ))
  else
    error("starter/init.lua: unknown provider kind: " .. tostring(p.kind))
  end
end

-- Pre-seed the reasoner-graph binary's peer set with every Lua-resident
-- reasoner type. Without this, a spawn_graph referencing a reasoner that
-- hasn't emitted a bus event yet fails with "reasoner not connected."
local rg_argv = { require("config").bin("reasoner-graph") }
do
  local reasoners_mod = require("reasoners")
  local known = reasoners_mod._internals and reasoners_mod._internals.handlers
  if type(known) == "table" then
    for name, _ in pairs(known) do
      rg_argv[#rg_argv + 1] = "--peer"
      rg_argv[#rg_argv + 1] = name
    end
  end
end
actor.spawn(require("compositors.graph").spawn_spec(rg_argv))

local tools = require("compositors.tools")
local tool_gate_argv = { require("config").bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.auto_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--auto"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

-- lead-workflow lives alongside agentic-loop, not inside it: separate
-- bus subscriptions, separate state. Owns plan/approval state and the
-- active graph run id; advertises mag / write-review /
-- await-approval to tool-gate. Registered BEFORE tool-gate's spawn so
-- its bus subscription is live when tool-gate.hello arrives —
-- otherwise the advertise is missed and the lead model gets "no such
-- tool" at runtime.
actor.spawn(require("lead-workflow"))

-- read-only-tools advertises list_dir + search_text (Lua-resident,
-- pure-read). Same ordering reason as lead-workflow: register before
-- tool-gate spawn so the gate's first hello triggers our advertise.
actor.spawn(require("read-only-tools"))

-- Tool-validator owns the chat.tool.permission_request → chat.tool.
-- popup_request translation: classifies bash commands through `da`
-- (approve/deny/defer), routes only the deferred ones to a user popup.
-- Must be spawned BEFORE tool-gate so its subscription is live when
-- the first gated invocation lands. The chat surface listens to
-- popup_request, not permission_request — without the validator
-- running, gated invocations never reach the popup.
actor.spawn(require("tool-validator"))

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.basic_actor_spec())

actor.spawn(require("compositors.chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat/init.lua",
}))

if startup.prompt ~= nil then
  local submitted = false
  nefor.bus.on_event("basic-tools.hello", function(_env)
    if submitted then return end
    submitted = true
    nefor.engine.send(nefor.json.encode({
      type = "event",
      from = "startup",
      ts   = nefor.engine.now(),
      body = { kind = "chat.input.submit", text = startup.prompt },
    }))
  end)
end
