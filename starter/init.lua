-- starter/init.lua — default engine composition.
--
-- Post Slice 2 I4 the engine is pure glue: no hardcoded NCP behavior, no
-- bundled widgets. This file is the canonical reference config:
--
--   1. Wire `package.path` so `require("ncp")` resolves to the bundled
--      protocol module next to this file.
--   2. Optionally declare a parent session id to resume from (commented out
--      by default — uncomment and fill in a uuid to continue a prior run).
--   3. Define the global `step` hook the engine calls on every inbound line.
--      Delegates to `ncp.step` — the protocol module is where the NCP v0.1
--      semantics live.
--   4. Register plugins via `nefor.plugins.spawn`. Mirrors the pre-split
--      reference config (`tmp/smoke-config-m2/init.lua`) plus the
--      combinators plugin; swap or remove entries to compose your own stack.
--
-- ### T7 — Stage 1 starter wiring
--
-- The chat plugin no longer talks to a provider directly. Instead:
--
--   chat.input.submit        → chat_orchestrator.lua → reasoner-graph.run
--   reasoner-graph dispatches → reasoner_graph_adapter.lua → openai-provider
--                                                         → tool-gate
--   chat.complete.result     → reasoner_graph_adapter   → graph.node_result
--   tool.result              → reasoner_graph_adapter   → graph.node_result
--   graph.run_complete       → chat_orchestrator       → chat.message.append
--
-- Three Lua glue modules co-attach to the reasoner-graph spawn:
--   * type adapter         — drives provider/tool work for each
--                            reasoner type (`dummy`, `provider-wrapper`,
--                            `tool-executor`, `adapter`, `terminal`).
--   * spawn_graph binding  — exposes `spawn_graph` as a tool in the
--                            orchestrator's catalog.
--   * chat orchestrator    — translates chat.input.submit ↔ reasoner-graph.
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
-- 2. Optional parent session id (resume a prior run)
-------------------------------------------------------------------------
-- nefor.parent_session = "00000000-0000-0000-0000-000000000000"

-------------------------------------------------------------------------
-- 3. Step function
-------------------------------------------------------------------------
local ncp = require("ncp")

function step(saved_log, current_log)
  ncp.step(saved_log, current_log)
end

-------------------------------------------------------------------------
-- 4. Plugin composition
-------------------------------------------------------------------------

local cc_adapter         = require("mock_plugin_adapter")
local mk_openai_provider = require("openai_provider_adapter").make
local rg_adapter         = require("reasoner_graph_adapter")
local spawn_graph        = require("spawn_graph")
local chat_orchestrator  = require("chat_orchestrator")

-- Compose two `{from_plugin, to_plugin}` transform pairs into one. The
-- inner adapter runs first; if it drops or rewrites the envelope the
-- outer adapter sees the result.
local function compose_adapters(inner, outer)
  local function chain_from(env)
    local e = env
    if inner.from_plugin then e = inner.from_plugin(e) end
    if e == nil then return nil end
    if outer.from_plugin then e = outer.from_plugin(e) end
    return e
  end
  local function chain_to(env)
    local e = env
    if outer.to_plugin then e = outer.to_plugin(e) end
    if e == nil then return nil end
    if inner.to_plugin then e = inner.to_plugin(e) end
    return e
  end
  return { from_plugin = chain_from, to_plugin = chain_to }
end

-- Compose a list of adapters, left-to-right (first is innermost). Each
-- adapter is `{from_plugin, to_plugin}`; missing fields default to
-- identity. `from_plugin` runs in list order; `to_plugin` runs in
-- reverse.
local function compose_chain(adapters)
  if #adapters == 0 then return {} end
  local result = adapters[1]
  for i = 2, #adapters do
    result = compose_adapters(result, adapters[i])
  end
  return result
end

-- Plugin cwd is <plugin_root>/<name>/ (engine policy).
local PROJECT_ROOT = STARTER_ROOT:match("^(.*)/[^/]+$") or "."
local function bin(name) return PROJECT_ROOT .. "/target/debug/" .. name end

local _ = cc_adapter  -- keep require warm; mock-plugin backend is opt-in

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
--   8. nefor-chat / nefor-tui  (UI; can come up any time)
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
-- 4b. Provider — openai-provider against local Ollama
-------------------------------------------------------------------------
--
-- One provider, one chat session per orchestrator instance. The
-- orchestrator's reasoner-graph adapter calls
-- `ollama.chat.create / chat.append / chat.complete` directly; the
-- legacy `chat.input.submit → ollama.prompt` adapter path is left
-- inert (we don't translate `chat.input.submit` here; the chat
-- orchestrator intercepts it on nefor-chat's egress before any
-- provider sees it).
--
-- The static_token=ollama-anything trick unlocks the openai-provider's
-- auth gate without a real key — required for local Ollama. Real
-- providers should provide an --api-key CLI arg instead.
local PROVIDER_NAME = "ollama"
local PROVIDER_MODEL = "gemma4:latest"

local provider_chat = mk_openai_provider(PROVIDER_NAME, { static_token = "ollama-local" })

ncp.spawn {
  name        = PROVIDER_NAME,
  command     = {
    bin("openai-provider"),
    "--name",     PROVIDER_NAME,
    "--base-url", "http://localhost:11434",
    "--model",    PROVIDER_MODEL,
  },
  from_plugin = compose_chain({
    -- Inner: type-adapter intercepts chat.complete.result for chats we own.
    rg_adapter.for_provider(PROVIDER_NAME),
    -- Outer: chat-contract translation (stream.delta → chat.stream.delta, …).
    provider_chat,
  }).from_plugin,
  to_plugin   = provider_chat.to_plugin,
}

-------------------------------------------------------------------------
-- 4c. Reasoner graph — three adapters co-attach
-------------------------------------------------------------------------
--
-- Order: type-adapter (innermost) → spawn_graph → chat orchestrator.
-- The type adapter handles `<reasoner>.run_node` egress (always); the
-- spawn_graph binding catches `graph.run_complete` for spawn_graph
-- sub-runs (matched by run_id) and emits `tool.result`; the chat
-- orchestrator catches `graph.run_complete` for chat-driven runs. The
-- gate-forwarded `spawn-graph-tool.tool.invoke` is caught on
-- tool-gate's egress chain (4d below), not here.

chat_orchestrator.configure {
  provider = PROVIDER_NAME,
  model    = PROVIDER_MODEL,
  -- Minimal system prompt: verifies the chat pipeline still honours
  -- terse user instructions when a system message is present. Tools
  -- are still advertised through the catalog (spawn_graph, basic
  -- tools), so this also tests instruction-following with tools
  -- attached. If gemma starts ignoring "respond X and nothing else"
  -- prompts again, the tool catalog is the next variable to isolate.
  system   = "You are a helpful assistant.",
}
rg_adapter.set_default_provider(PROVIDER_NAME, PROVIDER_MODEL)
-- next_state capture rides on rg_adapter's in-process observer hook.
-- A `to_plugin` transform on the reasoner-graph spawn would miss
-- `graph.node_result` envelopes — they're shipped via
-- `nefor.engine.send` from Lua and bypass the bus's transform stack.
chat_orchestrator.attach_state_capture()

local rg_chain = compose_chain({
  -- `for_starter()` is the innermost transform in the rg chain: it
  -- watches reasoner-graph's own `ready` egress and emits
  -- `reasoner-graph.register_reasoner { name }` for each Lua-resident
  -- reasoner type (provider-wrapper, tool-executor, adapter, terminal,
  -- dummy). Without this, the scheduler's connected-peer set never
  -- learns those names and synthesises "reasoner '<name>' not
  -- connected" on the first dispatch.
  rg_adapter.for_starter(),
  rg_adapter.for_reasoner_graph(),
  spawn_graph.for_reasoner_graph(),
  chat_orchestrator.for_reasoner_graph(),
})

ncp.spawn {
  name        = "reasoner-graph",
  command     = { bin("reasoner-graph") },
  from_plugin = rg_chain.from_plugin,
}

-------------------------------------------------------------------------
-- 4d. Tool gate + basic-tools + spawn_graph advertisement
-------------------------------------------------------------------------

local gate_chain = compose_chain({
  rg_adapter.for_tool_gate(),
  spawn_graph.for_tool_gate("tool-gate"),
})

ncp.spawn {
  name        = "tool-gate",
  command     = {
    bin("tool-gate"),
    "--prompt",  "read_file",
    "--default", "prompt",
  },
  from_plugin = gate_chain.from_plugin,
}

ncp.spawn {
  name    = "basic-tools",
  command = { bin("basic-tools"), "--gate", "tool-gate" },
}

-------------------------------------------------------------------------
-- 4e. Chat (must come up before chat orchestrator works, but the chain
--     is set on its spawn so registration is just here)
-------------------------------------------------------------------------

local chat_chain = compose_chain({
  chat_orchestrator.for_chat(),
})

ncp.spawn {
  name        = "nefor-chat",
  command     = { bin("nefor-chat") },
  from_plugin = chat_chain.from_plugin,
}

ncp.spawn {
  name    = "nefor-tui",
  command = { bin("nefor-tui") },
}
