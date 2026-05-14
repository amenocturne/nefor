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

package.path = table.concat({
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-------------------------------------------------------------------------
-- 2. Dispatch hook + session management
-------------------------------------------------------------------------

local ncp      = require("ncp")
local actor    = require("actor")
local sessions = require("sessions")

function dispatch(current_log)
  ncp.dispatch(current_log)
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
--   1. provider/tool contract declare()  (subscribe BEFORE combinators
--      spawns so we don't miss its `combinators.ready` event)
--   2. nefor-combinators       (registry)
--   3. agentic-loop            (orchestrator state machine)
--   4. reasoners               (Lua-resident reasoner handlers)
--   5. provider                (openai-provider or mock-plugin)
--   6. reasoner-graph          (queries combinators on submit)
--   7. tool-gate               (aggregates tool advertisements)
--   8. basic-tools             (advertises tools)
--   9. nefor-tui               (UI)

require("lib.contracts.provider").declare()
require("lib.contracts.tool").declare()

actor.spawn(require("nefor-combinators"))

-- Boot the orchestrator actor + resident reasoner handlers BEFORE
-- the plugins they coordinate. The actor runtime queues incoming
-- envelopes during boot, so even if a plugin's `ready` arrives
-- earlier than expected nothing is lost.
local agentic_loop = require("agentic-loop")
agentic_loop.configure {
  provider = cfg.provider.name,
  model    = cfg.provider.model,
  system   = [[
You are a helpful assistant. Use the `spawn_graph` tool for parallel decomposition tasks (multiple independent sub-questions to combine).

Graph schema:
{ "nodes": [{ "id": str, "reasoner": str, "args": {...} }], "edges": [{ "from": str, "to": str }] }

Reasoner types:
- `responder` — one-shot LLM call. args: { "prompt": string }. Upstream nodes' outputs become user messages prepended to the prompt.
- `terminal` — sink. args: {}. Exactly one per graph; its input becomes the run's result.

To combine parallel branches into a single output, add a `responder` combine node downstream of the parallel branches and feed it into terminal. Do NOT wire parallel branches directly into terminal — terminal is a sink, not a combiner. Pattern:
  branchA, branchB → combine (responder) → terminal

Emit the tool call directly after deciding the structure. For simple chat turns (no decomposition benefit), just answer directly.
]],
}
actor.spawn(agentic_loop)
actor.spawn(require("reasoners"))

-------------------------------------------------------------------------
-- 3b. Provider — selected by config.lua (prod = openai-provider, test = mock).
-------------------------------------------------------------------------

local PROVIDER_NAME  = cfg.provider.name
local PROVIDER_MODEL = cfg.provider.model

if cfg.plugins.spawn_mock then
  actor.spawn(require("mock-plugin").spawn_spec(
    PROVIDER_NAME,
    {
      require("config").bin("mock-plugin"),
      "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
    }
  ))
else
  local provider_command = {
    require("config").bin("openai-provider"),
    "--name",     PROVIDER_NAME,
    "--base-url", cfg.provider.base_url,
  }
  if PROVIDER_MODEL then
    table.insert(provider_command, "--model")
    table.insert(provider_command, PROVIDER_MODEL)
  end
  for _, a in ipairs(cfg.provider.extra_args or {}) do
    table.insert(provider_command, a)
  end
  actor.spawn(require("openai-provider").spawn_spec(
    PROVIDER_NAME,
    provider_command,
    { static_token = cfg.provider.static_token }
  ))
end

-------------------------------------------------------------------------
-- 3c. Reasoner graph
-------------------------------------------------------------------------

actor.spawn(require("reasoner-graph").spawn_spec({ require("config").bin("reasoner-graph") }))

-------------------------------------------------------------------------
-- 3d. Tool gate + basic-tools
-------------------------------------------------------------------------

local tool_gate_argv = { require("config").bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

actor.spawn(require("tool-gate").spawn_spec("tool-gate", tool_gate_argv))

actor.spawn(require("basic-tools"))

-------------------------------------------------------------------------
-- 3e. Chat (declarative TUI)
-------------------------------------------------------------------------

actor.spawn(require("nefor-tui").spawn_spec({
  require("config").bin("nefor-tui"),
  "--script", STARTER_ROOT .. "/chat.lua",
}))
