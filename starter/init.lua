-- starter/init.lua — default engine composition.
--
-- Post Phase 3a the orchestration glue lives in per-plugin actor
-- folders rather than a single `agentic_workflow.lua` module:
--
--   * `agentic-loop/`     — orchestrator state machine
--   * `reasoners/`        — Lua-resident reasoner type handlers
--                           (responder, terminal, tool-executor,
--                            adapter, provider-wrapper, dummy)
--   * `<plugin>/`         — wrapper actor per Rust binary, owning
--                           that plugin's wire-protocol translation
--                           (openai-provider, mock-plugin, tool-gate,
--                            nefor-tui, reasoner-graph,
--                            nefor-combinators, basic-tools)
--
-- This file just composes the actors in spawn order.
--
-- Run:
--   NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter

-------------------------------------------------------------------------
-- 1. Lua module path — bundled protocol + json alongside this file
-------------------------------------------------------------------------
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
-- lib. For in-tree development the `dir` override skips the clone
-- path; pm registers the dir and (idempotently) ensures package.path
-- covers each plugin lib's parent so bare `require("<name>")`
-- resolves to the plugin lib at `plugins/<name>/lua/<name>/init.lua`.
-- The starter-side composers live as per-domain files (chat_bridge,
-- provider, tools, graph, combinators) at the starter root.
local pm = require("nefor-pm")
pm.install({
  -- Multi-consumer protocol primitives. Sub-modules accessed as
  -- `require("core.envelope")`, `require("core.ncp")`, etc. The
  -- aggregator `require("core")` returns a table of all sub-modules.
  {
    "amenocturne/nefor",
    name = "core",
    tag  = "v0.1.5",
    path = "lua/core/",
    dir  = LUA_ROOT .. "/core",
  },

  -- Independent generic libs (no plugin binary, no cross-deps beyond
  -- core). Each lib lives at `libs/<name>/init.lua`; the umbrella
  -- entry grafts the parent dir so `require("libs.generic-provider")`
  -- / `require("libs.generic-tool")` resolve.
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

  -- reasoner-graph ships the `spawn_graph` protocol primitive only —
  -- the actor-spec wiring is identity passthrough and lives in
  -- starter/graph.lua via `core.actor.identity_spec`. `require(
  -- "reasoner-graph.spawn_graph")` resolves the sub-module.
  {
    "amenocturne/nefor",
    name = "reasoner-graph",
    tag  = "v0.1.5",
    path = "plugins/reasoner-graph/lua/reasoner-graph/",
    dir  = STARTER_ROOT .. "/../plugins/reasoner-graph/lua/reasoner-graph",
  },
})

-------------------------------------------------------------------------
-- 2. Dispatch hook + session management
-------------------------------------------------------------------------

local ncp       = require("core.ncp")
local actor     = require("core.actor")
local sessions  = require("sessions")
local cfg       = require("config").active
local lead_role = require("lead_role")

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()
actor.spawn(sessions)
sessions.init()

-------------------------------------------------------------------------
-- 3. Plugin composition
-------------------------------------------------------------------------
--
-- Order matters because plugins register types/Into declarations
-- against `nefor-combinators` at startup, and the scheduler queries
-- combinators at submit time.
--
--   1. provider/tool contract declare()  (eagerly emit type-tag
--      registrations onto the bus; combinators picks them up via
--      ncp.lua's replay-on-attach when it readies)
--   2. nefor-combinators       (registry)
--   3. agentic-loop            (orchestrator state machine)
--   4. reasoners               (Lua-resident reasoner handlers)
--   5. provider                (openai-provider or mock-plugin)
--   6. reasoner-graph          (queries combinators on submit)
--   7. tool-gate               (aggregates tool advertisements)
--   8. basic-tools             (advertises tools)
--   9. nefor-tui               (UI)

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

actor.spawn(require("combinators"))

-- Boot the orchestrator actor + resident reasoner handlers BEFORE
-- the plugins they coordinate. The actor runtime queues incoming
-- envelopes during boot, so even if a plugin's `ready` arrives
-- earlier than expected nothing is lost.
local agentic_loop = require("agentic-loop")
agentic_loop.configure {
  provider = cfg.default_provider,
  model    = cfg.default_model,
  system   = lead_role.LEAD_SYSTEM_PROMPT,
  -- Restrict the lead's chat catalog to the orchestration-tool surface.
  -- Without this filter the lead sees every wire-advertised tool — most
  -- problematically `spawn_graph` (the reasoner-graph internal that
  -- `dispatch-graph` translates into) — and can call them directly,
  -- bypassing the role-keyed sub-agent contract and bottoming out in
  -- `reasoner '<role>' not connected` runtime errors. The agent
  -- reasoner already enforces a per-role allowlist on its sub-firings
  -- via the same `chat.create.tools` plumbing; this extends the same
  -- discipline to the lead's chat at the orchestrator layer.
  tool_allowlist = lead_role.ORCHESTRATION_TOOLS,
}
actor.spawn(agentic_loop)
actor.spawn(require("reasoners"))

-------------------------------------------------------------------------
-- 3b. Providers — every entry in cfg.providers is spawned. The picker
--     aggregates connected providers via auth.status; no hard-coded
--     test/prod switch.
-------------------------------------------------------------------------

local provider = require("provider")
for _, p in ipairs(cfg.providers or {}) do
  if p.kind == "mock" then
    -- mock-plugin speaks the same wire protocol as the openai-provider
    -- binary, so the same actor spec works — only the binary command
    -- differs.
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

-------------------------------------------------------------------------
-- 3c. Reasoner graph
-------------------------------------------------------------------------

actor.spawn(require("graph").spawn_spec({ require("config").bin("reasoner-graph") }))

-------------------------------------------------------------------------
-- 3d. Tool gate + basic-tools
-------------------------------------------------------------------------

local tools = require("tools")
local tool_gate_argv = { require("config").bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))

actor.spawn(tools.basic_actor_spec())

-------------------------------------------------------------------------
-- 3d2. Lead-workflow actor — owns plan/approval state + active graph
--      run_id; advertises `dispatch-graph` / `write-review` /
--      `await-approval` tools to tool-gate. Lives alongside
--      `agentic-loop`, not inside it (separate bus subscriptions,
--      separate state).
-------------------------------------------------------------------------

actor.spawn(require("lead-workflow"))

-------------------------------------------------------------------------
-- 3e. Chat (declarative TUI)
-------------------------------------------------------------------------

actor.spawn(require("chat_bridge").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat.lua",
}))
