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

package.path = table.concat({
  CONFIG_ROOT .. "/?.lua",
  CONFIG_ROOT .. "/?/init.lua",
  STARTER_ROOT .. "/?.lua",
  STARTER_ROOT .. "/?/init.lua",
  package.path,
}, ";")

local ncp      = require("ncp")
local actor    = require("actor")
local sessions = require("sessions")

function dispatch(current_log)
  ncp.dispatch(current_log)
end

function invoke_from_plugin(source, payload)
  ncp.invoke_from_plugin(source, payload)
end

actor.install()
actor.spawn(sessions)
sessions.init()

local agentic_cli = require("agentic_cli")

local function bin(name) return PROJECT_ROOT .. "/target/debug/" .. name end

-- ------------------------------------------------------------------
-- Plugin spawn order (mirrors starter/init.lua minus chat/tui).
-- ------------------------------------------------------------------

require("lib.contracts.provider").declare()
require("lib.contracts.tool").declare()

actor.spawn(require("nefor-combinators"))

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

if cfg.plugins.spawn_mock then
  actor.spawn(require("mock-plugin").spawn_spec(
    PROVIDER_NAME,
    {
      bin("mock-plugin"),
      "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
    }
  ))
else
  local provider_command = {
    bin("openai-provider"),
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

actor.spawn(require("reasoner-graph").spawn_spec({ bin("reasoner-graph") }))

local tool_gate_argv = { bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

actor.spawn(require("tool-gate").spawn_spec("tool-gate", tool_gate_argv))

actor.spawn(require("basic-tools"))

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (actor.spawn / ncp.spawn don't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
