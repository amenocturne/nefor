-- starter/init.lua — default engine composition.
--
-- The starter is designed to be COPIED to ~/.config/nefor (`just
-- install` does this) without taking the rest of the repo with it.
-- That means relative paths into `../lua/` and `../plugins/` aren't
-- a usable source for nefor-pm: when the user runs from
-- ~/.config/nefor/, those paths resolve to ~/.config/lua/ etc., which
-- don't exist.
--
-- Bootstrap order:
--   1. NEFOR_DEV_DIR set     → in-checkout dev mode; resolve from there.
--   2. STARTER_ROOT/../lua/nefor-pm exists → in-checkout (running via
--                                            `just run`); use ../lua etc.
--   3. otherwise             → sparse-clone amenocturne/nefor at the
--                              pinned ref into <DATA>/nefor/, use that.
--
-- Once the bootstrap picks NEFOR_ROOT, every pm.install entry resolves
-- its `dir` from that single root — no more "../" walks from the
-- starter location.

local STARTER_ROOT = NEFOR_CONFIG_DIR or "."
local NEFOR_DEV_DIR = os.getenv("NEFOR_DEV_DIR")

-- Upstream ref derived from the engine's version. Exact release tags
-- (e.g. 0.1.9 → v0.1.9) pin Lua libs to the matching binary version;
-- nightly/dev builds fall back to "main". The clone path and every
-- pm.install entry below pick it up.
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

-- Resolve NEFOR_ROOT — the directory whose `lua/` and `plugins/`
-- mirror the github repo layout.
local NEFOR_ROOT
if NEFOR_DEV_DIR and #NEFOR_DEV_DIR > 0 then
  NEFOR_ROOT = NEFOR_DEV_DIR
elseif path_exists(STARTER_ROOT .. "/../lua/nefor-pm/init.lua") then
  NEFOR_ROOT = STARTER_ROOT .. "/.."
else
  local data_dir = nefor.fs.data_root()
  local pm_root  = data_dir .. "/nefor"
  if not path_exists(pm_root) then
    nefor.fs.mkdir_p(data_dir)
    local clone_cmd = "git clone --depth 1 --filter=blob:none --sparse "
                   .. "--branch '" .. UPSTREAM_REF .. "' "
                   .. "https://github.com/amenocturne/nefor.git '" .. pm_root .. "'"
    if not run(clone_cmd) then
      error("nefor bootstrap: git clone failed for ref " .. UPSTREAM_REF
            .. ". Check git is on PATH, the network is reachable, and the "
            .. "ref exists on origin.")
    end
  end
  if not run("git -C '" .. pm_root .. "' sparse-checkout set " .. SPARSE_CONE) then
    error("nefor bootstrap: git sparse-checkout failed for " .. pm_root)
  end
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
-- Defense-in-depth fallback for the synchronous `history_replay.set`
-- path that sessions drives around its replay burst. Wired explicitly
-- here so module load stays free of bus dependencies.
history_replay.install()
actor.spawn(sessions)
sessions.init()

-- Spawn order matters: provider/tool type-tag registrations must reach
-- nefor-combinators before the scheduler queries it on submit. Order:
--   1. libs.generic-{provider,tool}.declare()
--   2. compositors.combinators
--   3. agentic-loop + reasoners
--   4. providers
--   5. reasoner-graph + tool-gate + basic-tools
--   6. lead-workflow
--   7. chat (declarative TUI)

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

actor.spawn(require("compositors.combinators"))

-- The actor runtime queues incoming envelopes during boot, so spawning
-- the orchestrator and its resident reasoners before the plugins they
-- coordinate is safe even if a plugin's `ready` arrives early.
local agentic_loop = require("agentic-loop")
agentic_loop.configure {
  provider = cfg.default_provider,
  model    = cfg.default_model,
  system   = lead_role.LEAD_SYSTEM_PROMPT,
  -- Restrict the lead's chat catalog to the orchestration-tool surface.
  -- Without this filter the lead sees every wire-advertised tool — most
  -- problematically `spawn_graph` (the reasoner-graph internal that
  -- `dispatch-graph` translates into) — and could call them directly,
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

actor.spawn(require("compositors.graph").spawn_spec({ require("config").bin("reasoner-graph") }))

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
-- active graph run id; advertises dispatch-graph / write-review /
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

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv, { agentic_loop = agentic_loop }))
actor.spawn(tools.basic_actor_spec())

actor.spawn(require("compositors.chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat/init.lua",
}))
