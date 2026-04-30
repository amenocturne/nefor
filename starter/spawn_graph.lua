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
-- ### Async / non-blocking flow
--
-- spawn_graph is FIRE-AND-FORGET from the calling agent's perspective:
--
--   1. Model emits `tool-gate.tool.invoke { id, name="spawn_graph", args }`.
--   2. Gate forwards as `spawn-graph-tool.tool.invoke { id, ... }`.
--   3. We intercept at tool-gate's egress, mint `run_id`, emit
--      `reasoner-graph.run` to start the sub-graph, AND emit
--      `tool.result { id = gate_inner_id, output = "<ack text>" }`
--      immediately. The wrap node unblocks within milliseconds; the
--      orchestrator's chat run finishes after the model emits a
--      transitional turn ("started, will relay when ready"). The chat
--      UI is no longer pinned for the duration of the sub-graph.
--   4. When `graph.run_complete` arrives later we broadcast a new
--      event `spawn_graph.completed { run_id, status, output?, error? }`.
--      `chat_orchestrator` (the only v1 listener) picks it up and
--      injects a deferred-result message into the chat as a fresh turn.
--
-- This decouples the "tool call returns" semantics from "sub-graph
-- finishes". The tool-call return is purely an ack; the actual result
-- arrives as a new conversation turn, modeled honestly as a
-- user-injected message the model then relays back to the user.
--
-- ### Events emitted
--
-- `spawn_graph.completed` (broadcast):
--   { kind = "spawn_graph.completed",
--     run_id = "rg-...",
--     status = "success" | "failure" | "<other>",
--     output = "<combined text>",   -- on success
--     error  = "<error text>" }     -- on failure
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

-- In-process subscribers for sub-graph completion. Registered via
-- `M.on_completed(fn)`. Called synchronously inside `for_reasoner_graph`'s
-- handler when a sub-graph we own finishes. We use an in-process callback
-- (rather than relying on the broadcast `spawn_graph.completed` event)
-- because Lua-emitted bus events bypass plugin `from_plugin` transforms —
-- chat_orchestrator can't subscribe at the bus layer. The broadcast still
-- happens for visibility (session log, future plugin listeners).
local completed_subscribers = {}

-- Sub-graph dispatches captured from `tool-gate.tool.invoke` but not
-- yet emitted as `reasoner-graph.run`. rg_adapter releases this queue
-- when wrap-firing-2's first stream chunk arrives — see the comment
-- block above the invoke-handler emit-tool.result for why we defer.
-- Each entry: { run_id, graph, on_node_failure }.
local pending_dispatches = {}

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
--
-- Preference order:
--   1. `results.terminal.output.text` — canonical agent-style exit
--      (the user's graph included a terminal node, by convention).
--   2. Any node's `output.text` whose key contains "terminal", "out",
--      or "final" (catches conventional naming variations).
--   3. Any node's `output.text` (Lua `pairs` order is undefined; this
--      is a last-resort fallback for graphs without a designated exit).
--   4. JSON-encoded results map.
--
-- Without (1), a graph with multiple text-producing nodes would surface
-- whichever one `pairs` happened to visit first — non-deterministic and
-- usually wrong. The terminal-key preference makes the canonical pattern
-- deterministic.
local function extract_text(entry)
  if type(entry) ~= "table" or type(entry.output) ~= "table" then return nil end
  local out = entry.output
  return out.text or (out.final_answer and out.final_answer.text) or nil
end

local function serialise_results(results)
  if type(results) ~= "table" then return tostring(results) end

  local terminal_text = extract_text(results.terminal)
  if type(terminal_text) == "string" then return terminal_text end

  for nid, entry in pairs(results) do
    if type(nid) == "string"
        and (string.find(nid, "terminal") or string.find(nid, "out") or string.find(nid, "final")) then
      local txt = extract_text(entry)
      if type(txt) == "string" then return txt end
    end
  end

  for _, entry in pairs(results) do
    local txt = extract_text(entry)
    if type(txt) == "string" then return txt end
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

      -- Tool ack returns immediately so the model's wrap-firing-2 can
      -- start right away. The text after the marker is context: it
      -- goes into chat history so the model knows what it submitted,
      -- and the tagged-message hint primes it to recognise the
      -- deferred-result turn when it arrives. Brief-and-imperative
      -- ack constraints sit on the model side (system prompt would be
      -- the right home if we want them; the tool result itself stays
      -- declarative).
      emit(nil, {
        kind   = "tool.result",
        id     = invoke_id,
        output = "Submitted sub-graph run_id=" .. run_id ..
                 ". Acknowledge briefly to the user, or chain another " ..
                 "tool call. The real result will arrive later as a " ..
                 "user message tagged `[spawn_graph(run_id=" .. run_id ..
                 ") result]`.",
      })

      -- Queue the sub-graph dispatch instead of emitting it now.
      -- rg_adapter's stream.delta hook releases the queue once Ollama
      -- starts streaming wrap-firing-2's response — that guarantees
      -- the wrap request is committed at Ollama BEFORE the sub-graph
      -- requests queue behind it. Without this, a 50-token ack waits
      -- 60s+ behind the (typically 1500-token) sub-graph nodes
      -- because they reach Ollama's HTTP queue first.
      pending_dispatches[#pending_dispatches + 1] = {
        run_id          = run_id,
        graph           = graph,
        on_node_failure = on_failure,
      }

      nefor.log.info("spawn_graph: queued sub-graph dispatch (will flush on wrap stream.delta)", {
        run_id = run_id,
        gate_inner_id = invoke_id,
        queue_depth = #pending_dispatches,
      })
      return nil
    end

    return env
  end
  return { from_plugin = from_plugin }
end

-- Intercept `graph.run_complete` at reasoner-graph's egress for runs we
-- spawned. The `tool.result` was already emitted at invoke time as an
-- immediate ack (async semantics); on completion we broadcast a
-- `spawn_graph.completed` event for the chat orchestrator (or any
-- future listener) to inject the real result as a deferred turn.
-- Composed with the type-driven adapter on the reasoner-graph spawn in
-- init.lua.
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
      local completed = {
        kind   = "spawn_graph.completed",
        run_id = run_id,
        status = status,
      }
      if status == "success" then
        completed.output = serialise_results(env.body.results)
      else
        completed.error = "spawn_graph run completed with status `" .. status .. "`: " ..
                          json.encode(env.body.results or {})
      end
      nefor.log.info("spawn_graph: sub-graph completed", {
        run_id = run_id,
        status = status,
        subscribers = #completed_subscribers,
      })
      -- In-process notification first (chat_orchestrator subscribes
      -- here). pcall each so a misbehaving subscriber doesn't break
      -- the chain.
      for _, cb in ipairs(completed_subscribers) do
        pcall(cb, completed)
      end
      -- Broadcast for visibility / future bus-side listeners. Note:
      -- engine.send-broadcast events don't pass through plugin
      -- from_plugin chains, so this is observation-only in v1.
      emit(nil, completed)
      -- Pass the original graph.run_complete through as well — other
      -- plugins (e.g. the chat orchestrator) may also be matching on
      -- run_id.
      return env
    end

    return env
  end
  return { from_plugin = from_plugin }
end

-- Subscribe to sub-graph completion events. The callback receives a
-- table:
--   { kind   = "spawn_graph.completed",
--     run_id = string,
--     status = "success" | "failure" | "<other>",
--     output = string,    -- on success
--     error  = string }   -- on failure
-- Subscribers are invoked synchronously inside the from_plugin
-- interception of `graph.run_complete`. Errors are pcall'd; a faulty
-- subscriber cannot break the chain.
function M.on_completed(callback)
  assert(type(callback) == "function",
         "on_completed: callback must be a function")
  completed_subscribers[#completed_subscribers + 1] = callback
end

-- Cancel every in-flight sub-graph run we minted. Emits one
-- `reasoner-graph.graph.cancel { run_id }` per pending run and clears
-- the registry so late `graph.run_complete` events are ignored. Used
-- by chat_orchestrator on `chat.interrupt_all` (double-ESC). Returns
-- the count of runs cancelled — useful for UI feedback.
function M.cancel_all_pending_runs()
  local n = 0
  for run_id, _ in pairs(pending_runs) do
    emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = run_id })
    n = n + 1
  end
  pending_runs = {}
  -- Drop any queued-but-not-yet-dispatched runs too. They were captured
  -- via tool.invoke (so the calling agent already saw the tool ack)
  -- but hadn't been handed to reasoner-graph yet — no dispatch means
  -- nothing to cancel via graph.cancel; just discarding them is correct.
  pending_dispatches = {}
  return n
end

-- Release every queued sub-graph dispatch. Called by rg_adapter from
-- its for_provider stream.delta hook on the orchestrator's wrap chat —
-- the moment Ollama starts streaming wrap-firing-2 is the moment we
-- know wrap's HTTP request is committed and any subsequent chat.complete
-- queues behind it. Idempotent: empty queue → no-op. Safe to call from
-- any stream-visible delta.
function M.flush_pending_dispatches()
  if #pending_dispatches == 0 then return 0 end
  local n = #pending_dispatches
  local snapshot = pending_dispatches
  pending_dispatches = {}
  for _, entry in ipairs(snapshot) do
    nefor.log.info("spawn_graph: dispatching queued sub-graph", {
      run_id = entry.run_id,
    })
    emit("reasoner-graph", {
      kind            = "reasoner-graph.run",
      run_id          = entry.run_id,
      graph           = entry.graph,
      on_node_failure = entry.on_node_failure,
    })
  end
  return n
end

-- Test-only state reset.
function M._reset()
  pending_runs = {}
  completed_subscribers = {}
  pending_dispatches = {}
end

return M
