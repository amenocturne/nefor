-- cli-config/init.lua — engine composition for the agentic-cli plugin.
--
-- Mirrors `starter/init.lua` but:
--   * No nefor-tui, no nefor-chat (the CLI surface IS stdout).
--   * Registers a virtual `agentic-cli` plugin via nefor.plugins.spawn
--     with a `cli` field; the engine dispatches to it via
--     `nefor plugin agentic-cli [args...]`.
--   * Keeps the same providers / reasoner-graph / tool-gate / basic-tools
--     stack so the agentic_workflow runs identically — behaviour parity
--     with the TUI is by construction.
--
-- Run:
--   ./target/debug/nefor --config cli-config/ plugin agentic-cli "your prompt"
--   NEFOR_CONFIG=test ./target/debug/nefor --config cli-config/ plugin agentic-cli "..."
-- (USE_MOCK_PROVIDER=true is still honored as a deprecated alias for
--  NEFOR_CONFIG=test — see cli-config/config.lua.)

local CONFIG_ROOT = NEFOR_CONFIG_DIR or "."

-- Reuse the modules that live in starter/. No symlinks required: just
-- add starter/ to the package path so require() resolves there.
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
local cfg      = require("config").active

-- Forward the engine's `current_log` to ncp.dispatch. The engine is
-- session-blind; cross-run resume + jsonl persistence are owned by
-- the sessions actor (registered with the actor runtime below).
function dispatch(current_log)
  ncp.dispatch(current_log)
end

-- Install the actor runtime, register the sessions actor, then mint a
-- fresh session. Done before any plugin spawn so the persistence path
-- is in place when the first envelope routes. Shutdown handling is
-- wired implicitly via the runtime's engine.shutdown synthesis.
actor.install()
actor.spawn(sessions)
sessions.init()

local agentic_workflow = require("agentic_workflow")
local agentic_cli      = require("agentic_cli")

local function bin(name) return PROJECT_ROOT .. "/target/debug/" .. name end

-- ------------------------------------------------------------------
-- Provider selection — driven by config.lua (cfg.provider + cfg.plugins).
-- ------------------------------------------------------------------

local PROVIDER_NAME  = cfg.provider.name
local PROVIDER_MODEL = cfg.provider.model
local provider_chain, provider_command

if cfg.plugins.spawn_mock then
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME)
  provider_command = {
    bin("mock-plugin"),
    "--script", STARTER_ROOT .. "/" .. cfg.provider.mock_script,
  }
else
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME, {
    static_token = cfg.provider.static_token,
  })
  provider_command = {
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
end

-- ------------------------------------------------------------------
-- Orchestrator setup — same prompt as starter/init.lua.
-- ------------------------------------------------------------------

local ORCHESTRATOR_SYSTEM_PROMPT = [[
You are a helpful assistant. Use the `spawn_graph` tool for parallel decomposition tasks (multiple independent sub-questions to combine).

Graph schema:
{ "nodes": [{ "id": str, "reasoner": str, "args": {...} }], "edges": [{ "from": str, "to": str }] }

Reasoner types:
- `responder` — one-shot LLM call. args: { "prompt": string }. Upstream nodes' outputs become user messages prepended to the prompt.
- `terminal` — sink. args: {}. Exactly one per graph; its input becomes the run's result.

To combine parallel branches into a single output, add a `responder` combine node downstream of the parallel branches and feed it into terminal. Do NOT wire parallel branches directly into terminal — terminal is a sink, not a combiner. Pattern:
  branchA, branchB → combine (responder) → terminal

Emit the tool call directly after deciding the structure. For simple chat turns (no decomposition benefit), just answer directly.
]]

agentic_workflow.setup {
  provider = PROVIDER_NAME,
  model    = PROVIDER_MODEL,
  system   = ORCHESTRATOR_SYSTEM_PROMPT,
}

-- ------------------------------------------------------------------
-- Plugin spawn order (mirrors starter/init.lua minus chat/tui).
-- ------------------------------------------------------------------

ncp.spawn { name = "nefor-combinators", command = { bin("nefor-combinators") } }
ncp.spawn { name = "generic-provider",  command = { bin("generic-provider")  } }
ncp.spawn { name = "generic-tool",      command = { bin("generic-tool")      } }

ncp.spawn {
  name        = PROVIDER_NAME,
  command     = provider_command,
  from_plugin = provider_chain.from_plugin,
  to_plugin   = provider_chain.to_plugin,
}

ncp.spawn {
  name        = "reasoner-graph",
  command     = { bin("reasoner-graph") },
  from_plugin = agentic_workflow.for_reasoner_graph().from_plugin,
}

local tool_gate_argv = { bin("tool-gate") }
for _, t in ipairs(cfg.tool_gate.prompt_tools or {}) do
  tool_gate_argv[#tool_gate_argv + 1] = "--prompt"
  tool_gate_argv[#tool_gate_argv + 1] = t
end
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

ncp.spawn {
  name        = "tool-gate",
  command     = tool_gate_argv,
  from_plugin = agentic_workflow.for_tool_gate("tool-gate").from_plugin,
}

actor.spawn(require("basic-tools")({ bin = bin("basic-tools") }))

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (ncp.spawn doesn't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
