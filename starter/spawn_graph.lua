-- starter/spawn_graph.lua — Lua binding that exposes `spawn_graph` as a
-- tool the orchestrator's provider can call (per parent spec §4 +
-- §6.1 Stage 1 bullet 6).
--
-- ### Wire shape
--
-- The tool's parameters mirror the `reasoner-graph.run` submission:
--   {
--     "graph": { "nodes": [...], "edges": [...] },
--     "on_node_failure": "abort" | "continue"  -- optional
--   }
--
-- Flow:
--   1. The model-side openai-provider chooses to call spawn_graph and
--      sends `tool-gate.tool.invoke { id, name = "spawn_graph", args }`.
--   2. The tool-gate forwards to `<source>.tool.invoke`. Our source
--      name is `spawn-graph-tool` — a virtual source registered via
--      `tools.advertise` at boot.
--   3. We intercept the forwarded `spawn-graph-tool.tool.invoke` at
--      tool-gate's egress (composed into the gate's `from_plugin`
--      chain in init.lua), translate to `reasoner-graph.run`, and
--      remember `<gate_inner_id> → <run_id>` correlation. Returning
--      `nil` from the transform drops the envelope so it never reaches
--      ncp.lua's targeted-routing — and there is no `spawn-graph-tool`
--      plugin to route it to anyway.
--   4. When `graph.run_complete` arrives (emitted by reasoner-graph)
--      we match `run_id` against pending runs, serialise the results
--      into a string, and emit `tool.result { id = gate_inner_id,
--      output }`. The `graph.run_complete` interception lives on
--      reasoner-graph's `from_plugin` chain — that IS where it's
--      emitted, so `from_plugin` is the right hook there.
--
-- ### Why a virtual source name (D-22)
--
-- An earlier draft advertised `spawn_graph` with `source =
-- "reasoner-graph"`, intending to intercept the gate-forwarded
-- `reasoner-graph.tool.invoke` on reasoner-graph's `from_plugin`
-- chain. That misroutes: ncp.lua's `from_plugin` runs at the *source*
-- plugin's egress, and the kind is emitted by **tool-gate** (during
-- forwarding), then targeted-routed to reasoner-graph by ncp.lua's
-- prefix rule. reasoner-graph (a Rust plugin) doesn't recognise the
-- kind and silently drops it; spawn_graph's transform never fires.
--
-- Naming the source `spawn-graph-tool` (a pseudo-plugin that doesn't
-- actually run as a binary) makes the gate forward
-- `spawn-graph-tool.tool.invoke`. ncp.lua's targeted-routing fails to
-- find a `spawn-graph-tool` plugin and falls through to broadcast,
-- but our `from_plugin` transform on **tool-gate's** egress catches
-- the kind first and returns nil to drop it. Per D-22, this is a
-- per-plugin envelope transform owning its own namespace rather than
-- a global namespace rename.
--
-- ### Stage 1 scope
--
-- Per parent spec §6.2 (Stage 2), recursive `spawn_graph` is deferred.
-- This Stage 1 binding does NOT prevent recursion structurally, but if
-- a spawned graph itself contains a node calling spawn_graph, the
-- inner run_id will collide with nothing (UUIDs are unique) — recursion
-- works mechanically. The "deferred" part is that we haven't validated
-- nested recursion semantically. Stage 1 happy path: orchestrator
-- emits ONE spawn_graph per turn.

local M = {}

local json = nefor.json

-- pending_runs[run_id] = { gate_inner_id }
local pending_runs = {}

-- Build the JSON-schema parameters block for the spawn_graph tool.
-- The schema mirrors the reasoner-graph submission shape but kept
-- loose (Object) so the model can submit arbitrary graphs.
local function spawn_graph_schema()
  return {
    type = "object",
    description = "Submit a reasoner-graph run and return its results.",
    properties = {
      graph = {
        type = "object",
        description = "The graph topology: { nodes: [...], edges: [...] }.",
      },
      on_node_failure = {
        type = "string",
        enum = { "abort", "continue" },
        description = "Failure policy. Defaults to abort.",
      },
    },
    required = { "graph" },
  }
end

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function emit(target, body)
  local payload = json.encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = body,
  })
  if target ~= nil then
    nefor.engine.send(payload, target)
  else
    for _, peer in ipairs(nefor.engine.plugins()) do
      nefor.engine.send(payload, peer)
    end
  end
end

local function uuid_lite()
  -- Deterministic-enough id for Stage 1. Real uniqueness would use a
  -- proper UUID; the engine doesn't expose one to Lua and the
  -- scheduler accepts any string.
  return string.format(
    "rg-%d-%d",
    os.time(),
    math.random(0, 2 ^ 31 - 1)
  )
end

-- Serialise a `graph.run_complete.results` map into a tool-friendly
-- string. The orchestrator's provider gets this back as the spawn_graph
-- tool's `output` and includes it in the next chat turn.
local function serialise_results(results)
  if type(results) ~= "table" then return tostring(results) end
  -- Hunt for a FinalAnswer-shaped node (the canonical agent-style
  -- exit). Fall back to dumping the whole results map as JSON.
  for _, entry in pairs(results) do
    if type(entry) == "table" and type(entry.output) == "table" then
      local final = entry.output.text or (entry.output.final_answer and entry.output.final_answer.text)
      if type(final) == "string" then
        return final
      end
    end
  end
  return json.encode(results)
end

-- ------------------------------------------------------------------
-- public API
-- ------------------------------------------------------------------

-- The virtual source name we register `spawn_graph` under. There is no
-- plugin by this name on the bus — the gate-forwarded
-- `spawn-graph-tool.tool.invoke` is intercepted by `for_tool_gate`
-- below and dropped before targeting tries to deliver it.
local SPAWN_GRAPH_SOURCE = "spawn-graph-tool"

-- Build the advertise body. Used by the gate-side transform when
-- tool-gate.hello arrives — we don't have a startup hook so we
-- piggyback on the first event the gate emits to know it's up.
local function advertise_body(gate_name)
  return {
    kind   = gate_name .. ".tools.advertise",
    source = SPAWN_GRAPH_SOURCE,
    tools  = {
      {
        name        = "spawn_graph",
        description = "Submit a reasoner-graph run and return its terminal results.",
        parameters  = spawn_graph_schema(),
      },
    },
  }
end

-- Attach to the tool-gate spawn. Two responsibilities on the gate's
-- egress:
--   (a) advertise the `spawn_graph` tool on the first `tool-gate.hello`
--       emission. We can't advertise from init.lua's top-level because
--       no plugin has attached to the bus yet; emit-on-hello is the
--       minimum-friction equivalent of a "post-ready" hook.
--   (b) intercept the gate-forwarded `spawn-graph-tool.tool.invoke`
--       (the gate emits this when our advertised tool gets called).
--       Translate to `reasoner-graph.run` and drop the envelope so
--       ncp.lua's targeted-routing doesn't try to deliver it to a
--       non-existent `spawn-graph-tool` plugin.
function M.for_tool_gate(gate_name)
  gate_name = gate_name or "tool-gate"
  local advertised = false
  local hello_kind = gate_name .. ".hello"
  local invoke_kind = SPAWN_GRAPH_SOURCE .. ".tool.invoke"
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    if not advertised and env.body.kind == hello_kind then
      advertised = true
      emit(gate_name, advertise_body(gate_name))
      return env
    end

    if env.body.kind == invoke_kind then
      local name = env.body.name
      local invoke_id = env.body.id
      local args = env.body.args or {}
      -- Defensive: only `spawn_graph` lives under this source. If
      -- something else somehow showed up, drop it (returning the
      -- envelope would broadcast it to every peer, which is louder
      -- than dropping with a `tool.result` error).
      if name ~= "spawn_graph" or type(invoke_id) ~= "string" then
        return nil
      end

      local graph = args.graph
      local on_failure = args.on_node_failure or "abort"
      if type(graph) ~= "table" then
        emit(nil, {
          kind  = "tool.result",
          id    = invoke_id,
          error = "spawn_graph: missing or non-object `graph` argument",
        })
        return nil
      end

      local run_id = uuid_lite()
      pending_runs[run_id] = { gate_inner_id = invoke_id }

      emit("reasoner-graph", {
        kind            = "reasoner-graph.run",
        run_id          = run_id,
        graph           = graph,
        on_node_failure = on_failure,
      })
      return nil
    end

    return env
  end
  return { from_plugin = from_plugin }
end

-- Intercept `graph.run_complete` at reasoner-graph's egress for runs we
-- spawned. Translates the terminal results into a `tool.result` for the
-- gate inner id we remembered at invoke time. Composed with the
-- type-driven adapter on the reasoner-graph spawn in init.lua.
function M.for_reasoner_graph()
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind
    if type(kind) ~= "string" then return env end

    if kind == "graph.run_complete" then
      local run_id = env.body.run_id
      local pending = pending_runs[run_id]
      if not pending then return env end  -- not ours
      pending_runs[run_id] = nil

      local status = env.body.status or "unknown"
      local body = {
        kind = "tool.result",
        id   = pending.gate_inner_id,
      }
      if status == "success" then
        body.output = serialise_results(env.body.results)
      else
        body.error = "spawn_graph run completed with status `" .. status .. "`: " ..
                     json.encode(env.body.results or {})
      end
      emit(nil, body)
      -- Pass the original graph.run_complete through as well — other
      -- plugins (e.g. the chat orchestrator) may also be matching on
      -- run_id.
      return env
    end

    return env
  end
  return { from_plugin = from_plugin }
end

-- Test-only state reset.
function M._reset()
  pending_runs = {}
end

return M
