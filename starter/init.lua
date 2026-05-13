-- starter/init.lua — default engine composition.
--
-- Run:
--   NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter

local STARTER_ROOT = NEFOR_CONFIG_DIR or "."
local LUA_ROOT = STARTER_ROOT .. "/../lua"

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm wires the core primitives, generic libs, and every plugin
-- lib. The `dir` override skips the clone path for in-tree builds; pm
-- registers each dir and ensures package.path covers it so bare
-- `require("<name>")` resolves to the plugin lib.
local pm = require("nefor-pm")
pm.install({
  {
    "amenocturne/nefor",
    name = "core",
    tag  = "v0.1.5",
    path = "lua/core/",
    dir  = LUA_ROOT .. "/core",
  },

  {
    "amenocturne/nefor",
    name = "libs",
    tag  = "v0.1.5",
    path = "lua/libs/",
    dir  = LUA_ROOT .. "/libs",
  },

  {
    "amenocturne/nefor",
    name = "openai-provider",
    tag  = "v0.1.5",
    path = "plugins/openai-provider/lua/openai-provider/",
    dir  = STARTER_ROOT .. "/../plugins/openai-provider/lua/openai-provider",
  },

  {
    "amenocturne/nefor",
    name = "tool-gate",
    tag  = "v0.1.5",
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = STARTER_ROOT .. "/../plugins/tool-gate/lua/tool-gate",
  },

  {
    "amenocturne/nefor",
    name = "nefor-tui",
    tag  = "v0.1.5",
    path = "plugins/nefor-tui/lua/",
    dir  = STARTER_ROOT .. "/../plugins/nefor-tui/lua",
  },

  {
    "amenocturne/nefor",
    name = "reasoner-graph",
    tag  = "v0.1.5",
    path = "plugins/reasoner-graph/lua/reasoner-graph/",
    dir  = STARTER_ROOT .. "/../plugins/reasoner-graph/lua/reasoner-graph",
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
      }
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
      { static_token = p.static_token }
    ))
  else
    error("starter/init.lua: unknown provider kind: " .. tostring(p.kind))
  end
end

actor.spawn(require("compositors.graph").spawn_spec({ require("config").bin("reasoner-graph") }))

local tools = require("compositors.tools")
local tool_gate_argv = { require("config").bin("tool-gate") }
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

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.basic_actor_spec())

actor.spawn(require("compositors.chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat/init.lua",
}))
