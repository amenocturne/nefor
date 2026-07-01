-- cli-config/init.lua — engine composition for the agentic-cli plugin.
--
-- Mirrors `starter/init.lua` post-Phase-3a but:
--   * No nefor-tui (the CLI surface IS stdout).
--   * Registers a virtual `agentic-cli` plugin via nefor.plugins.spawn
--     directly (the engine dispatches to it via
--     `nefor plugin agentic-cli [args...]`).
--
-- Run:
--   ./target/debug/nefor --config cli-config/ plugin agentic-cli "your prompt"
--   NEFOR_CONFIG=test ./target/debug/nefor --config cli-config/ plugin agentic-cli "..."

local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."

-- Reuse the modules that live in starter/. Add starter/ to the
-- package path so require() resolves there.
local PROJECT_ROOT = CONFIG_ROOT:match("^(.*)/[^/]+$") or "."
local STARTER_ROOT = PROJECT_ROOT .. "/starter"
local LUA_ROOT = PROJECT_ROOT .. "/lua"

package.path = table.concat({
  CONFIG_ROOT .. "/?.lua",
  CONFIG_ROOT .. "/?/init.lua",
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  LUA_ROOT .. "/?.lua",
  LUA_ROOT .. "/?/init.lua",
  package.path,
}, ";")

-- nefor-pm wires the core primitives, generic libs, and every plugin
-- lib. The `dir` overrides skip the clone path; pm registers each dir
-- and puts it on package.path so `require("<name>")` resolves to the
-- plugin lib. Starter composers live as per-domain files (provider,
-- tools, graph) at the starter root and are reached via
-- plain `require("<name>")`.
local pm = require("nefor-pm")
pm.install({
  -- Multi-consumer protocol primitives.
  {
    "amenocturne/nefor",
    name = "core",
    tag  = "v0.1.5",
    path = "lua/core/",
    dir  = LUA_ROOT .. "/core",
  },

  -- Independent generic libs (no plugin binary, no cross-deps beyond core).
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
    dir  = PROJECT_ROOT .. "/plugins/openai-provider/lua/openai-provider",
  },

  {
    "amenocturne/nefor",
    name = "tool-gate",
    tag  = "v0.1.5",
    path = "plugins/tool-gate/lua/tool-gate/",
    dir  = PROJECT_ROOT .. "/plugins/tool-gate/lua/tool-gate",
  },

  -- reasoner-graph's actor-spec wiring is identity passthrough and
  -- lives in starter/graph.lua via `core.actor.identity_spec`. The
  -- `spawn_graph` protocol contract lives at `libs.spawn-graph`.
  {
    "amenocturne/nefor",
    name = "reasoner-graph",
    tag  = "v0.1.5",
    path = "plugins/reasoner-graph/lua/reasoner-graph/",
    dir  = PROJECT_ROOT .. "/plugins/reasoner-graph/lua/reasoner-graph",
  },
})

local ncp      = require("core.ncp")
local actor    = require("core.actor")
local sessions = require("sessions")
local cfg      = require("config").active

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()

actor.spawn(sessions)
sessions.init()

local agentic_cli = require("cli")

-- ------------------------------------------------------------------
-- Plugin spawn order (mirrors starter/init.lua minus chat/tui).
-- ------------------------------------------------------------------

require("libs.generic-provider").declare()
require("libs.generic-tool").declare()

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

local PROVIDER_NAME  = cfg.provider.name
local PROVIDER_MODEL = cfg.provider.model

local provider = require("compositors.provider")
if cfg.plugins.spawn_mock then
  -- mock-plugin uses the same wire protocol as the openai-provider
  -- binary, so the provider actor spec works as-is.
  actor.spawn(provider.spawn_spec(
    PROVIDER_NAME,
    {
      require("config").bin("mock-plugin"),
      "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
    },
    { agentic_loop = agentic_loop }
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
  actor.spawn(provider.spawn_spec(
    PROVIDER_NAME,
    provider_command,
    { static_token = cfg.provider.static_token, agentic_loop = agentic_loop }
  ))
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

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv, { agentic_loop = agentic_loop }))

actor.spawn(tools.basic_actor_spec())

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (actor.spawn / ncp.spawn don't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
