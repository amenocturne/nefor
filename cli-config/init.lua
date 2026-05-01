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
--   USE_MOCK_PROVIDER=true ./target/debug/nefor --config cli-config/ plugin agentic-cli "..."

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

local ncp = require("ncp")

function step(saved_log, current_log)
  ncp.step(saved_log, current_log)
end

local agentic_workflow = require("agentic_workflow")
local agentic_cli      = require("agentic_cli")

local function bin(name) return PROJECT_ROOT .. "/target/debug/" .. name end

-- ------------------------------------------------------------------
-- Provider selection (USE_MOCK_PROVIDER=true swaps in the deterministic
-- mock; same branching as starter/init.lua).
-- ------------------------------------------------------------------

local USE_MOCK_PROVIDER = (os.getenv("USE_MOCK_PROVIDER") == "true")

local PROVIDER_NAME, PROVIDER_MODEL, provider_chain, provider_command

if USE_MOCK_PROVIDER then
  PROVIDER_NAME  = "mock-plugin"
  PROVIDER_MODEL = "mock-model"
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME)
  provider_command = {
    bin("mock-plugin"),
    "--script", STARTER_ROOT .. "/mock_provider.lua",
  }
else
  PROVIDER_NAME  = "ollama"
  PROVIDER_MODEL = "qwen3.6:35b-a3b-coding-mxfp8"
  provider_chain = agentic_workflow.for_provider(PROVIDER_NAME, { static_token = "ollama-local" })
  provider_command = {
    bin("openai-provider"),
    "--name",     PROVIDER_NAME,
    "--base-url", "http://localhost:11434",
    "--model",    PROVIDER_MODEL,
  }
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

ncp.spawn {
  name        = "tool-gate",
  command     = {
    bin("tool-gate"),
    -- CLI surface has no permission-prompt UI in v1. Default `auto` keeps
    -- the agent unblocked; --yolo on agentic-cli is the documented user
    -- override (currently a placeholder, see agentic_workflow.set_yolo).
    -- Phase 3 / Stage 2 should add a prompt-respecting CLI surface and
    -- flip this back to `prompt`.
    "--default", "auto",
  },
  from_plugin = agentic_workflow.for_tool_gate("tool-gate").from_plugin,
}

ncp.spawn { name = "basic-tools", command = { bin("basic-tools"), "--gate", "tool-gate" } }

-- ------------------------------------------------------------------
-- Virtual agentic-cli plugin — calls nefor.plugins.spawn directly to
-- pass the `cli` field (ncp.spawn doesn't accept it).
-- ------------------------------------------------------------------

nefor.plugins.spawn {
  name = "agentic-cli",
  cli  = function(argv) return agentic_cli.run(argv) end,
}
