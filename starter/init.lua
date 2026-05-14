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

local ncp      = require("ncp")
local sessions = require("sessions")
local cfg      = require("config").active

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
-- 3b. Providers — every entry in cfg.providers is spawned. The picker
--     aggregates connected providers via auth.status; no hard-coded
--     test/prod switch.
-------------------------------------------------------------------------

for _, p in ipairs(cfg.providers or {}) do
  if p.kind == "mock" then
    actor.spawn(require("mock-plugin").spawn_spec(
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
    actor.spawn(require("openai-provider").spawn_spec(
      p.name,
      provider_command,
      { static_token = p.static_token }
    ))
  else
    error("starter/init.lua: unknown provider kind: " .. tostring(p.kind))
  end
end

actor.spawn(require("compositors.graph").spawn_spec({ require("config").bin("reasoner-graph") }))

ncp.spawn {
  name    = "nefor-combinators",
  command = { bin("nefor-combinators") },
}

ncp.spawn {
  name    = "generic-provider",
  command = { bin("generic-provider") },
}

ncp.spawn {
  name    = "generic-tool",
  command = { bin("generic-tool") },
}

-------------------------------------------------------------------------
-- 4b. Provider — selected by config.lua (prod = openai-provider, test = mock).
-------------------------------------------------------------------------
--
-- The `prod` table runs openai-provider against the configured base_url
-- (Ollama by default). The `test` table runs mock-plugin loading
-- `mock_provider.lua` for deterministic smoke / e2e tests. Both emit
-- the same `<name>.chat.*` / `<name>.stream.*` envelope shape so the
-- rest of the composition is provider-agnostic.
--
-- The `static_token=ollama-local` trick on prod unlocks openai-provider's
-- auth gate without a real key (required for local Ollama). Real remote
-- providers would supply an --api-key CLI arg via provider.extra_args.

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
tool_gate_argv[#tool_gate_argv + 1] = "--default"
tool_gate_argv[#tool_gate_argv + 1] = cfg.tool_gate.default_action

actor.spawn(tools.gate_spec("tool-gate", tool_gate_argv))
actor.spawn(tools.basic_actor_spec())

-- lead-workflow lives alongside agentic-loop, not inside it: separate
-- bus subscriptions, separate state. Owns plan/approval state and the
-- active graph run id; advertises dispatch-graph / write-review /
-- await-approval to tool-gate.
actor.spawn(require("lead-workflow"))

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

ncp.spawn {
  name        = PROVIDER_NAME,
  command     = provider_command,
  from_plugin = provider_chain.from_plugin,
  to_plugin   = provider_chain.to_plugin,
}

-------------------------------------------------------------------------
-- 4d. Reasoner graph
-------------------------------------------------------------------------

ncp.spawn {
  name        = "reasoner-graph",
  command     = { bin("reasoner-graph") },
  from_plugin = agentic_workflow.for_reasoner_graph().from_plugin,
}

-------------------------------------------------------------------------
-- 4e. Tool gate + basic-tools + spawn_graph advertisement
-------------------------------------------------------------------------

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

ncp.spawn {
  name    = "basic-tools",
  command = { bin("basic-tools"), "--gate", "tool-gate" },
}

-------------------------------------------------------------------------
-- 4f. Chat
-------------------------------------------------------------------------
--
-- Post-phase-6 cutover: the chat surface is a Lua composition (`chat.lua`)
-- running inside the new declarative `nefor-tui` plugin. The plugin loads
-- the script via `--script <path>` and exposes a `tui.*` primitive surface
-- (text, column, row, scrollable, text_input, markdown, ...) that
-- `chat.lua` composes into the transcript + statusline + input. The
-- legacy split (`nefor-chat` + ratatui-based `nefor-tui`) is gone.

ncp.spawn {
  name        = "nefor-tui",
  command     = { bin("nefor-tui"), "--script", STARTER_ROOT .. "/chat.lua" },
  from_plugin = agentic_workflow.for_chat().from_plugin,
}
