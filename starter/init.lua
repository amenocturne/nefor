-- starter/init.lua — default engine composition.
--
-- Post Slice 2 I4 the engine is pure glue: no hardcoded NCP behavior, no
-- bundled widgets. This file is the canonical reference config:
--
--   1. Wire `package.path` so `require("ncp")` resolves to the bundled
--      protocol module next to this file.
--   2. Define the global `step` hook the engine calls on every inbound line
--      and bring up the sessions module (boot + shutdown wiring). Session
--      continuity is composed in Lua — see `starter/sessions.lua`.
--   3. Register plugins via `nefor.plugins.spawn`. Mirrors the pre-split
--      reference config (`tmp/smoke-config-m2/init.lua`) plus the
--      combinators plugin; swap or remove entries to compose your own stack.
--
-- ### T7 — Stage 1 starter wiring (post-Phase-1B)
--
-- The chat plugin no longer talks to a provider directly. Instead a single
-- `agentic_workflow` module owns the orchestration glue: it intercepts
-- `chat.input.submit`, drives the reasoner-graph against the provider via
-- a template orchestrator graph (provider-wrapper + tool-executor +
-- adapter + terminal cycle), wires the spawn_graph tool, and surfaces
-- run completions back to nefor-chat. See
-- `starter/agentic_workflow.lua` for the full event flow.
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
--
-- Session continuity (boot, shutdown, resume) is composed entirely in
-- Lua via the sessions module. The Rust engine is session-blind: it
-- forwards inbound lines to the dispatch hook, broadcasts events, and
-- exits on request. Session id minting, on-disk jsonl persistence, and
-- in-process resume all live in `starter/sessions.lua`.
--
-- The legacy sidechannel-write + process-exit + engine-restart flow
-- (and the `nefor.parent_session` engine handoff that drove it) are
-- gone — they killed the TUI on every resume. The new flow is
-- pure-Lua, in-process, message-bus juggling. See sessions.lua's
-- docstring for the bus protocol.

local ncp      = require("ncp")
local actor    = require("actor")
local sessions = require("sessions")
local cfg      = require("config").active

-- Forward the engine's `current_log` to ncp.dispatch. The engine is
-- session-blind: cross-run resume is owned by `starter/sessions.lua`,
-- which is now an actor registered with `actor.lua`. The actor runtime
-- subscribes to the bus once and routes every envelope to all spawned
-- actors' `receive_msg`.
function dispatch(current_log)
  ncp.dispatch(current_log)
end

-- Install the actor runtime — subscribes to the bus and to engine
-- shutdown lifecycle. Done BEFORE actor.spawn so the bus subscription
-- is live when actors start running their boot code.
actor.install()

-- Register the sessions actor. The actor's `receive_msg` handles
-- persistence, resume_request, new_request, and engine.shutdown via
-- the bus — no separate bus or lifecycle subscriptions in the module.
actor.spawn(sessions)

-- Mint a fresh session and emit `sessions.session_start`. Done before
-- any plugin spawn so the persistence path is in place when the first
-- envelope routes. Shutdown handling is wired implicitly via the actor
-- runtime — no `sessions.handle_shutdown()` call needed.
sessions.init()

-------------------------------------------------------------------------
-- 4. Plugin composition
-------------------------------------------------------------------------

local agentic_workflow = require("agentic_workflow")

local function bin(name)
  local plugin_dir = os.getenv("NEFOR_PLUGIN_DIR")
  if plugin_dir and plugin_dir ~= "" then
    return plugin_dir .. "/" .. name
  end
  error("NEFOR_PLUGIN_DIR is not set. The engine resolves this automatically; if you see this error, set it manually or pass --plugin-dir.")
end

-------------------------------------------------------------------------
-- 4a. Spawn order
-------------------------------------------------------------------------
--
-- Order matters because plugins register types/Into declarations
-- against `nefor-combinators` at startup, and the scheduler queries
-- combinators at submit time. The safe order:
--
--   1. nefor-combinators       (registry)
--   2. generic-provider        (canonical type tags)
--   3. generic-tool            (canonical type tags)
--   4. openai-provider(s)      (declare Into against canonical types)
--   5. reasoner-graph          (queries combinators on submit)
--   6. tool-gate               (aggregates tool advertisements)
--   7. basic-tools             (advertises tools to the gate)
--   8. nefor-tui                (UI; can come up any time — chat is a Lua composition inside it)
--
-- ncp.lua's replay-on-attach means a late-attaching plugin still sees
-- prior events, so this ordering is a robustness measure rather than a
-- hard correctness requirement. It's still worth respecting because
-- the combinators registry is queried synchronously during submit —
-- if reasoner-graph submitted a graph before combinators readied, the
-- query would block on a peer that doesn't exist yet.

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

-------------------------------------------------------------------------
-- 4c. Orchestrator setup — single configuration call
-------------------------------------------------------------------------
--
-- Stage-1 system prompt: teaches the orchestrator model when and how
-- to use `spawn_graph`. Kept terse on purpose — Gemma 3 reasons itself
-- into a "stop" finish without committing to the tool call when the
-- prompt is dense. Schema-only worked-example was enough to make it
-- emit a well-formed graph reliably; the verbose version was not.
-- Two reasoner types are documented because those are the ones
-- agentic_workflow handles for sub-graphs (`responder` = one-shot LLM,
-- `terminal` = sink); other reasoner types are private to the
-- orchestrator's chat loop and would just confuse the model.
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
