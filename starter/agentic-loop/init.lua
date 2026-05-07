-- starter/agentic-loop/init.lua — orchestrator actor.
--
-- ## Layout
--
-- This plugin is a folder. `init.lua` (this file) is the production
-- actor — `require("agentic-loop")` returns it. Test escape hatches
-- live in `agentic-loop/test.lua` if needed.
--
-- ## Shape
--
-- Returns the actor table — `{ name, receive_msg, send_msg, ..., public API }`.
-- The starter registers it via `actor.spawn(require("agentic-loop"))`
-- alongside the per-plugin wrapper actors.
--
-- ## What this actor owns
--
-- All orchestrator state that used to live at module scope in
-- `starter/agentic_workflow.lua`:
--
--   * `config` — provider/model/system seeds for the orchestrator
--     template graph
--   * `current_run_id`, `current_state` — single-flight orchestrator
--     run tracking
--   * `pending`, `pending_runs`, `pending_dispatches` — in-flight firings,
--     spawn_graph runs, queued sub-graph dispatches
--   * `chat_id_to_key`, `tool_id_to_key`, `chat_id_stream_visible` —
--     reverse maps for plugin-replies that don't carry firing_id, plus
--     D-26 stream gating
--   * `deferred_queue`, `pending_user_inputs` — spawn_graph completions
--     and busy-time submits
--   * Public observer lists (on_stream/on_reasoning/on_tool_start/...)
--
-- ## Interplay with per-plugin wrappers
--
-- The wrapper folders (`openai-provider/`, `tool-gate/`, `reasoner-graph/`,
-- `nefor-tui/`) own the wire-protocol translation between each plugin's
-- native envelope shape and the canonical bus shape. Post Phase 3b the
-- canonical wire is `tool.invoke` / `tool.result` — reasoner-graph
-- (the Rust binary) dispatches nodes via `tool.invoke { id=firing_id,
-- name=<reasoner>, args }`, expects `tool.result { id=firing_id, result }`
-- back, and closes the run with `tool.result { id=run_id, result: { status,
-- results } }`. `graph.node.fired` is the paired observer envelope
-- broadcast alongside each `tool.invoke` so observers can map firing_id
-- → (run_id, node_id, reasoner) without parsing dispatch traffic.
--
-- The agentic-loop dispatches on these inbound kinds:
--
--   * `chat.input.submit { text }`        — user submit; build orchestrator
--                                           graph + emit canonical
--                                           tool.invoke{name=spawn_graph}
--   * `chat.interrupt_all`                — double-Esc; cancel everything
--   * `chat.reset`                        — /new: clear state + broadcast
--   * `chat.model.set`                    — runtime model switch
--   * `graph.run_started { run_id, total_nodes }` — observability only
--   * `graph.node.fired { run_id, node_id, firing_id, reasoner }` —
--                                           observer paired with each
--                                           tool.invoke; agentic-loop uses
--                                           it to map firing_id → node_id
--                                           for the orchestrator run so
--                                           `tool.result` for the wrap
--                                           firing can capture next_state
--   * `tool.result { id, result | error }` — three flavours, disambiguated
--                                           by `id`:
--                                              - `id == current_run_id`     → orchestrator complete
--                                              - `id` in `pending_runs`     → sub-graph complete
--                                              - `id` in firing→node map   → wrap-node next_state capture
--                                           Other tool.result envelopes (real
--                                           tool returns, spawn_graph acks)
--                                           pass through to other consumers.
--   * `<provider>.chat.complete.result`   — translated by provider wrapper into
--                                           a `tool.result` keyed by
--                                           firing_id. The wrapper does the
--                                           chat_id → firing lookup +
--                                           emission.
--   * `<provider>.stream.delta` etc.      — observed via wrapper translations
--                                           (fire stream/reasoning observers)
--   * `tool-gate.tool.invoke` (spawn_graph) — gate-forwarded tool invocation;
--                                            handled by per-plugin tool-gate
--                                            wrapper for the spawn_graph case
--   * `sessions.session_start/end/resume_done` — replay lifecycle
--   * `engine.shutdown`                   — no-op (sessions handles)

local json = nefor.json

local envelope      = require("lib.envelope")
local ids           = require("lib.ids")
local results_lib   = require("lib.results")
local graph_lib     = require("lib.graph")
local replay_window = require("lib.replay_window")

-- ------------------------------------------------------------------
-- module-private state — held in `state` table; mutations are explicit
-- ------------------------------------------------------------------

local state = {
  -- Orchestrator config — mutated by configure() / chat.model.set.
  config = {
    provider = "ollama",
    model    = nil,
    system   = nil,
  },

  current_run_id = nil,         ---@type string|nil
  current_state  = nil,         ---@type table|nil   -- next_state from wrap
  deferred_queue       = {},    ---@type table       -- queued spawn_graph results { text }
  pending_user_inputs  = {},    ---@type table       -- queued submits while busy
  pending              = {},    ---@type table       -- run_id:firing_id → entry
  pending_runs         = {},    ---@type table       -- sub-graph run_id → meta
  pending_dispatches   = {},    ---@type table       -- queued sub-graph dispatches (D-31)
  chat_id_to_key       = {},    ---@type table
  tool_id_to_key       = {},    ---@type table
  chat_id_stream_visible = {},  ---@type table
  -- firing_id → { run_id, node_id, reasoner } for tracked runs (the
  -- orchestrator run + every sub-graph run we own). Populated by
  -- graph.node.fired observer envelopes; consumed when a tool.result
  -- arrives for a firing_id we care about (today: only wrap-node
  -- next_state capture).
  firing_to_node       = {},    ---@type table

  -- Observer registries. Public on_* setters append; producers fire via
  -- pcall so a bad observer doesn't break the chain.
  stream_observers       = {},  ---@type table
  reasoning_observers    = {},  ---@type table
  tool_start_observers   = {},  ---@type table
  tool_end_observers     = {},  ---@type table
  complete_observers     = {},  ---@type table
  popup_observers        = {},  ---@type table
}

-- Reasoner types whose streaming should reach nefor-tui.
local STREAM_VISIBLE_TYPES = { ["provider-wrapper"] = true }

local SPAWN_GRAPH_SOURCE = graph_lib.SPAWN_GRAPH_SOURCE

local emit           = envelope.emit
local emit_to        = envelope.emit_to
local emit_broadcast = envelope.emit_broadcast
local pending_key    = ids.pending_key
local uuid_lite      = envelope.uuid_lite

local serialise_results = results_lib.serialise_results
local format_deferred   = results_lib.format_deferred

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function fire_observers(list, ...)
  for _, cb in ipairs(list) do pcall(cb, ...) end
end

-- ------------------------------------------------------------------
-- D-31 / cancel / dispatch helpers
-- ------------------------------------------------------------------

-- Release every queued sub-graph dispatch. Idempotent. D-31.
local function flush_pending_dispatches()
  if #state.pending_dispatches == 0 then return 0 end
  local n = #state.pending_dispatches
  local snapshot = state.pending_dispatches
  state.pending_dispatches = {}
  for _, entry in ipairs(snapshot) do
    nefor.log.info("agentic-loop: dispatching queued sub-graph", {
      run_id = entry.run_id,
    })
    -- Canonical tool contract: submit a graph by issuing a
    -- `tool.invoke { id=run_id, name="spawn_graph", args: { graph,
    -- on_node_failure } }`. The reasoner-graph binary parses this in
    -- its dispatch_event when name matches SPAWN_GRAPH_TOOL_NAME.
    emit("reasoner-graph", {
      kind = "tool.invoke",
      id   = entry.run_id,
      name = "spawn_graph",
      args = {
        graph           = entry.graph,
        on_node_failure = entry.on_node_failure,
      },
    })
  end
  return n
end

local function cancel_all_pending_runs()
  local n = 0
  for run_id, _ in pairs(state.pending_runs) do
    emit("reasoner-graph", { kind = "graph.cancel", run_id = run_id })
    n = n + 1
  end
  state.pending_runs = {}
  state.pending_dispatches = {}
  return n
end

-- ------------------------------------------------------------------
-- run lifecycle — submit / flush
-- ------------------------------------------------------------------

local function submit_orchestrator_run(user_text)
  if state.current_run_id ~= nil then return nil end

  local g = graph_lib.build_orchestrator_graph({
    provider  = state.config.provider,
    model     = state.config.model,
    system    = state.config.system,
    user_text = user_text or "",
  })

  if type(state.current_state) == "table"
      and type(state.current_state.chat_id) == "string" then
    g.nodes[1].args.seed_chat_id = state.current_state.chat_id
  end

  state.current_run_id = ids.mint_chat_run_id()
  -- Canonical tool contract: graph submission is a `tool.invoke` with
  -- name="spawn_graph". The reasoner-graph binary's dispatch_event
  -- routes by name and parses graph + policy from inside `args`.
  emit("reasoner-graph", {
    kind = "tool.invoke",
    id   = state.current_run_id,
    name = "spawn_graph",
    args = {
      graph           = g,
      on_node_failure = "abort",
    },
  })
  return state.current_run_id
end

local function flush_deferred()
  if state.current_run_id ~= nil then return end
  if #state.deferred_queue == 0 then return end
  local entry = table.remove(state.deferred_queue, 1)
  nefor.log.info("agentic-loop: flushing deferred spawn_graph result", {
    text_preview = string.sub(entry.text, 1, 80),
    queue_remaining = #state.deferred_queue,
  })
  submit_orchestrator_run(entry.text)
end

local function flush_pending_user_inputs()
  if state.current_run_id ~= nil then return end
  if #state.pending_user_inputs == 0 then return end
  local text = table.remove(state.pending_user_inputs, 1)
  nefor.log.info("agentic-loop: flushing queued user input", {
    text_preview = string.sub(text, 1, 80),
    queue_remaining = #state.pending_user_inputs,
  })
  submit_orchestrator_run(text)
end

-- Cancel everything. D-32 fan-out order:
--   (1) cancel current orchestrator run + interrupt the in-flight
--       provider stream so deltas stop spilling
--   (2) cancel sub-graph runs + clear queued dispatches
--   (3) drop deferred queue + pending user inputs
--   (4) clear pending bookkeeping
--
-- The single-Esc `cancel()` already fires `<provider>.interrupt` so
-- the binary aborts the streaming chat completion; `cancel_all` now
-- matches. Without it, `graph.cancel` on the reasoner-graph side is
-- "accept-and-drop" (the in-flight provider chat is not torn down by
-- the scheduler today), so an interrupt-all triggered by `/new`
-- mid-stream would let the prior turn's provider deltas keep
-- arriving and paint into the freshly-cleared transcript. The
-- `chat.reset` that sessions emits later in the `/new` path
-- translates to `<provider>.reset` which only clears chat history —
-- it does NOT interrupt the live turn — so the provider-interrupt
-- has to ride here.
local function cancel_all()
  local cancelled_chat = state.current_run_id ~= nil
  if cancelled_chat then
    emit("reasoner-graph", {
      kind   = "graph.cancel",
      run_id = state.current_run_id,
    })
    emit(state.config.provider, {
      kind = state.config.provider .. ".interrupt",
    })
    state.current_run_id = nil
  end
  local sub_n = cancel_all_pending_runs()
  local dropped = #state.deferred_queue
  local dropped_inputs = #state.pending_user_inputs
  state.deferred_queue = {}
  state.pending_user_inputs = {}
  state.pending = {}
  state.chat_id_to_key = {}
  state.chat_id_stream_visible = {}
  state.tool_id_to_key = {}
  state.firing_to_node = {}
  nefor.log.info("agentic-loop: cancel_all", {
    cancelled_chat_run = cancelled_chat,
    cancelled_sub_runs = sub_n,
    dropped_deferred = dropped,
    dropped_pending_inputs = dropped_inputs,
  })
  return {
    chat = cancelled_chat,
    sub_graphs = sub_n,
    deferred = dropped,
    pending_inputs = dropped_inputs,
  }
end

-- /new handler.
local function new_chat()
  state.current_state = nil
  state.current_run_id = nil
  state.deferred_queue = {}
  state.pending_user_inputs = {}
  emit(nil, { kind = "chat.reset" })
end

local function set_model(provider, model)
  if type(provider) == "string" and #provider > 0 then
    state.config.provider = provider
  end
  if type(model) == "string" and #model > 0 then
    state.config.model = model
  end
end

local function set_yolo(enabled)
  local default = enabled and "auto" or "prompt"
  emit("tool-gate", {
    kind    = "tool-gate.policy.set",
    default = default,
  })
  nefor.log.info("agentic-loop.set_yolo: placeholder event emitted", {
    enabled = enabled, default = default,
  })
end

-- Single-Esc behaviour: cancel the current chat turn at the provider.
local function cancel()
  if state.current_run_id == nil then return end
  emit(state.config.provider, {
    kind = state.config.provider .. ".interrupt",
  })
end

-- ------------------------------------------------------------------
-- envelope handlers — chat.input.submit / chat.reset / chat.model.set
-- ------------------------------------------------------------------

local function handle_chat_input_submit(body)
  local text = body.text or ""
  if type(text) ~= "string" or #text == 0 then return end

  nefor.log.info("agentic-loop: chat.input.submit received", {
    text_len = #text,
    text_preview = string.sub(text, 1, 80),
    busy = state.current_run_id ~= nil,
    deferred_queued = #state.deferred_queue,
    user_queued = #state.pending_user_inputs,
  })

  -- Echo the user message to the TUI as a transcript-bound event so
  -- replay can repaint user turns (chat.lua dedupes against the local
  -- push, so live view sees the user line exactly once).
  emit("nefor-tui", {
    kind = "chat.message.append",
    role = "user",
    text = text,
  })

  if state.current_run_id ~= nil then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = text
    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "system",
      text = string.format(
        "[queued — will dispatch when current turn finishes (%d in queue)]",
        #state.pending_user_inputs),
    })
    return
  end

  submit_orchestrator_run(text)
end

local function handle_chat_reset()
  nefor.log.info("agentic-loop: chat.reset received, clearing current_state", {
    had_state = state.current_state ~= nil,
    prior_chat_id = type(state.current_state) == "table" and state.current_state.chat_id or nil,
    dropped_deferred = #state.deferred_queue,
    dropped_pending_inputs = #state.pending_user_inputs,
    had_run = state.current_run_id ~= nil,
  })
  state.current_state = nil
  state.current_run_id = nil
  state.deferred_queue = {}
  state.pending_user_inputs = {}
end

local function handle_chat_model_set(body)
  local model = body.model
  local provider = body.provider
  if type(model) == "string" and #model > 0 then
    nefor.log.info("agentic-loop: chat.model.set received", {
      provider = provider, model = model, previous = state.config.model,
    })
    set_model(provider, model)
  end
end

-- ------------------------------------------------------------------
-- envelope handlers — graph.node.fired + tool.result
-- ------------------------------------------------------------------

-- graph.node.fired observer: track firing_id → (run_id, node_id) for
-- runs we care about (current orchestrator run + every sub-graph run
-- we own). The Rust scheduler emits one of these per dispatch alongside
-- the targeted tool.invoke; we use it to correlate the eventual
-- tool.result back to a node so the wrap-node next_state capture works.
local function handle_graph_node_fired(body)
  local run_id = body.run_id
  local firing_id = body.firing_id
  local node_id = body.node_id
  if type(run_id) ~= "string" or type(firing_id) ~= "string" then return end
  if run_id ~= state.current_run_id and state.pending_runs[run_id] == nil then
    return
  end
  state.firing_to_node[firing_id] = {
    run_id   = run_id,
    node_id  = node_id,
    reasoner = body.reasoner,
  }
end

-- A tool.result with id == one of our tracked run_ids closes that run.
local function handle_tool_result_run_close(run_id, body)
  -- tool.result { id=run_id, result: { status, results } } — Rust packs
  -- the prior `graph.run_complete` shape verbatim into result.
  local result = body.result
  local status, results
  if type(result) == "table" then
    status  = result.status
    results = result.results
  end
  results = results or {}

  -- Sub-graph completion: surface the literal sub-graph output to the
  -- user, then queue the LLM-instruction-wrapped relay text + flush.
  --
  -- The deferred relay text built by `format_deferred` is shaped for the
  -- LLM (it leads with "[spawn_graph(...) result]" + behavioural framing
  -- + the actual output). It rides as the next orchestrator-turn user
  -- message so the model can acknowledge / paraphrase. But the literal
  -- sub-graph terminal output is the ground truth — the user wants to
  -- see it directly, between the collapsed `▶ spawn_graph(...)` tool
  -- view and the model's relay paragraph. Emit it as a transcript-bound
  -- system message so it lands in the chat scrollback (and persists for
  -- replay) at the moment the sub-graph closes.
  local sub_pending = state.pending_runs[run_id]
  if sub_pending ~= nil then
    state.pending_runs[run_id] = nil
    local effective_status = status or "unknown"
    local completion = {
      kind   = "spawn_graph.completed",
      run_id = run_id,
      status = effective_status,
    }
    if effective_status == "success" then
      completion.output = serialise_results(results)
    else
      completion.error = "spawn_graph run completed with status `" .. effective_status ..
                         "`: " .. json.encode(results)
    end
    nefor.log.info("agentic-loop: sub-graph completed", {
      run_id = run_id, status = effective_status,
    })
    -- Surface the sub-graph terminal output (or error) literally in the
    -- chat transcript. We pick `chat.message.append` because it goes
    -- through the same persist+replay path as any other transcript
    -- entry; render shape is plain text with a system style.
    local visible_text
    if effective_status == "success" then
      visible_text = tostring(completion.output or "")
    else
      visible_text = "[spawn_graph errored] " ..
                     tostring(completion.error or "unknown error")
    end
    if #visible_text > 0 then
      emit("nefor-tui", {
        kind = "chat.message.append",
        role = "system",
        text = visible_text,
      })
    end
    local text = format_deferred(completion)
    state.deferred_queue[#state.deferred_queue + 1] = { text = text }
    flush_deferred()
    -- Note: do NOT return; the orchestrator-match branch below may
    -- also fire if (rare) the sub-graph completion races the
    -- orchestrator's own run-close envelope.
  end

  -- Orchestrator completion: clear current_run_id, surface results.
  if run_id == state.current_run_id then
    nefor.log.info("agentic-loop: tool.result run-close for our run", {
      run_id = run_id, status = status,
      had_state = state.current_state ~= nil,
      chat_id = type(state.current_state) == "table" and state.current_state.chat_id or nil,
      deferred_queued = #state.deferred_queue,
    })
    state.current_run_id = nil
    -- Drop firing→node mappings owned by this run.
    for fid, ref in pairs(state.firing_to_node) do
      if ref.run_id == run_id then state.firing_to_node[fid] = nil end
    end

    fire_observers(state.complete_observers, run_id, tostring(status))

    if status == "success" then
      flush_deferred()
      flush_pending_user_inputs()
      return
    end

    local err_text
    for _, key in ipairs({ "_typecheck", "_missing_combinators", "_error", "_cycle" }) do
      local entry = results[key]
      if type(entry) == "table" and type(entry.error) == "string" then
        err_text = "[" .. key .. "] " .. entry.error
        break
      end
    end
    if err_text == nil then
      for nid, entry in pairs(results) do
        if type(entry) == "table" and type(entry.error) == "string" then
          err_text = "[" .. tostring(nid) .. " errored] " .. entry.error
          break
        end
      end
    end
    if type(err_text) ~= "string" or #err_text == 0 then
      err_text = "[orchestrator finished with status: " .. tostring(status) .. "]"
    end

    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "system",
      text = err_text,
    })
    flush_deferred()
    flush_pending_user_inputs()
  end
end

-- Per-firing tool.result close: capture wrap node's next_state →
-- current_state for chat continuity. Matches behaviour of the prior
-- graph.node_result handler — only the wrap-firing's next_state matters
-- to the orchestrator.
local function handle_tool_result_firing_close(firing_id, body)
  local ref = state.firing_to_node[firing_id]
  if ref == nil then return end
  -- Drop the mapping; firing is closed.
  state.firing_to_node[firing_id] = nil
  if ref.run_id ~= state.current_run_id then return end
  if ref.node_id ~= "wrap" then return end
  -- next_state lives inside `result` per the wire-protocol spec
  -- (coordination point 1).
  local result = body.result
  if type(result) ~= "table" then return end
  local next_state = result.next_state
  if type(next_state) ~= "table" then return end
  state.current_state = next_state
  nefor.log.info("agentic-loop: captured next_state from wrap", {
    run_id  = ref.run_id,
    chat_id = next_state.chat_id,
  })
end

-- Top-level dispatcher for tool.result. Disambiguates by `id`:
--   - matches a tracked run_id           → run close
--   - matches a tracked firing_id        → wrap-node next_state capture
--   - neither (real tool, spawn_graph ack, etc.) → ignore
local function handle_tool_result(body)
  local id = body.id
  if type(id) ~= "string" or id == "" then return end
  if id == state.current_run_id or state.pending_runs[id] ~= nil then
    handle_tool_result_run_close(id, body)
    return
  end
  if state.firing_to_node[id] ~= nil then
    handle_tool_result_firing_close(id, body)
    return
  end
end

-- ------------------------------------------------------------------
-- session lifecycle
-- ------------------------------------------------------------------

local function teardown_for_session_end()
  if state.current_run_id ~= nil then
    emit("reasoner-graph", {
      kind = "graph.cancel",
      run_id = state.current_run_id,
    })
    state.current_run_id = nil
  end
  for run_id, _ in pairs(state.pending_runs) do
    emit("reasoner-graph", { kind = "graph.cancel", run_id = run_id })
  end
  state.pending_runs       = {}
  state.pending_dispatches = {}
  state.pending            = {}
  state.chat_id_to_key     = {}
  state.chat_id_stream_visible = {}
  state.tool_id_to_key     = {}
  state.firing_to_node     = {}
  state.current_state      = nil
  state.deferred_queue     = {}
  state.pending_user_inputs = {}
  emit(nil, { kind = "chat.reset" })
  nefor.log.info("agentic-loop: sessions.session_end → state cleared", {})
end

-- ------------------------------------------------------------------
-- sub-graph dispatch hook (used by tool-gate wrapper)
-- ------------------------------------------------------------------

-- Queue a sub-graph dispatch and return the minted run_id. Called by
-- the tool-gate wrapper when it intercepts the gate-forwarded
-- spawn_graph invocation. The dispatch is held in pending_dispatches
-- (D-31) and released on first wrap-stream delta, or via the backup
-- path on chat.complete.result.
local function queue_sub_graph(args, gate_inner_id)
  local g = args.graph
  local on_failure = args.on_node_failure or "abort"
  if type(g) ~= "table" then
    return nil, "spawn_graph: missing or non-object `graph` argument"
  end
  local run_id = uuid_lite()
  state.pending_runs[run_id] = { gate_inner_id = gate_inner_id }
  state.pending_dispatches[#state.pending_dispatches + 1] = {
    run_id          = run_id,
    graph           = g,
    on_node_failure = on_failure,
  }
  nefor.log.info("agentic-loop: queued sub-graph dispatch (will flush on wrap stream.delta)", {
    run_id = run_id,
    gate_inner_id = gate_inner_id,
    queue_depth = #state.pending_dispatches,
  })
  return run_id
end

-- Tool-executor pending entry constructor — called by the tool-executor
-- resident reasoner when it dispatches per-call invocations and needs to
-- correlate results back to its node firing. The shape is the same as
-- the pre-Phase-3 `pending[key]` for tool-executor:
--   { type, run_id, node_id, firing_id, reasoner, tool_calls, tool_results,
--     tool_ids, pending_count }
local function track_tool_executor(run_id, node_id, firing_id, calls, tool_ids)
  local key = pending_key(run_id, firing_id)
  state.pending[key] = {
    type          = "tool-executor",
    run_id        = run_id,
    node_id       = node_id,
    firing_id     = firing_id,
    reasoner      = "tool-executor",
    tool_calls    = calls,
    tool_results  = {},
    tool_ids      = tool_ids,
    pending_count = #calls,
  }
  for i, tid in ipairs(tool_ids) do
    state.tool_id_to_key[tid] = { key = key, idx = i }
  end
  return key
end

-- Provider-node pending entry constructor — same idea for the provider/
-- responder/wrapper/dummy reasoners.
local function track_provider_firing(reasoner_type, run_id, node_id, firing_id,
                                     provider_name, chat_id)
  local key = pending_key(run_id, firing_id)
  state.pending[key] = {
    type          = reasoner_type,
    run_id        = run_id,
    node_id       = node_id,
    firing_id     = firing_id,
    reasoner      = reasoner_type,
    provider_name = provider_name,
    chat_id       = chat_id,
  }
  state.chat_id_to_key[chat_id] = key
  state.chat_id_stream_visible[chat_id] = STREAM_VISIBLE_TYPES[reasoner_type] == true
  return key
end

-- Look up + clear pending entry by chat_id. Returns the entry or nil.
local function take_pending_for_chat(chat_id)
  if type(chat_id) ~= "string" then return nil end
  local key = state.chat_id_to_key[chat_id]
  if not key then return nil end
  local entry = state.pending[key]
  state.pending[key] = nil
  state.chat_id_to_key[chat_id] = nil
  return entry
end

-- Look up pending entry by chat_id without removing it.
local function peek_pending_for_chat(chat_id)
  if type(chat_id) ~= "string" then return nil end
  local key = state.chat_id_to_key[chat_id]
  if not key then return nil end
  return state.pending[key]
end

-- Stream-visible check by chat_id (D-26 gate).
local function stream_visible(chat_id)
  return state.chat_id_stream_visible[chat_id] == true
end

-- Tool-result correlation: look up by tool_id, returns
-- { key, idx, entry } or nil. Caller decrements pending_count and
-- emits node_result when zero.
local function take_pending_for_tool(tool_id)
  if type(tool_id) ~= "string" then return nil end
  local ref = state.tool_id_to_key[tool_id]
  if not ref then return nil end
  local entry = state.pending[ref.key]
  if not entry then
    state.tool_id_to_key[tool_id] = nil
    return nil
  end
  state.tool_id_to_key[tool_id] = nil
  return ref, entry
end

local function clear_pending_key(key)
  if state.pending[key] then state.pending[key] = nil end
end

-- Fire stream / reasoning observers (used by per-provider wrapper to
-- forward visible deltas to public observer registries).
local function fire_stream_observers(text)
  fire_observers(state.stream_observers, text)
end

local function fire_reasoning_observers(text)
  fire_observers(state.reasoning_observers, text)
end

local function fire_tool_start_observers(id, name, input)
  fire_observers(state.tool_start_observers, id, name, input)
end

local function fire_tool_end_observers(id, output, err)
  fire_observers(state.tool_end_observers, id, output, err)
end

-- ------------------------------------------------------------------
-- module-level exports made available to wrappers + reasoner actors
-- via `require("agentic-loop").<helper>`. Centralising the state-
-- mutation surface here keeps the wrappers structurally simple
-- (pure translation; no module-private state of their own) and
-- keeps the agentic-loop the single owner of orchestrator state.
-- ------------------------------------------------------------------

local M = {}

-- Public API (consumed by agentic_cli.lua + chat.lua).
function M.submit(text, _opts) return submit_orchestrator_run(text) end
function M.cancel()      cancel() end
function M.cancel_all()  return cancel_all() end
function M.new_chat()    new_chat() end
function M.set_model(provider, model) set_model(provider, model) end
function M.set_yolo(enabled) set_yolo(enabled) end

function M.on_stream(fn)
  assert(type(fn) == "function", "on_stream: callback must be a function")
  state.stream_observers[#state.stream_observers + 1] = fn
end
function M.on_reasoning(fn)
  assert(type(fn) == "function", "on_reasoning: callback must be a function")
  state.reasoning_observers[#state.reasoning_observers + 1] = fn
end
function M.on_tool_start(fn)
  assert(type(fn) == "function", "on_tool_start: callback must be a function")
  state.tool_start_observers[#state.tool_start_observers + 1] = fn
end
function M.on_tool_end(fn)
  assert(type(fn) == "function", "on_tool_end: callback must be a function")
  state.tool_end_observers[#state.tool_end_observers + 1] = fn
end
function M.on_complete(fn)
  assert(type(fn) == "function", "on_complete: callback must be a function")
  state.complete_observers[#state.complete_observers + 1] = fn
end
function M.on_popup(fn)
  assert(type(fn) == "function", "on_popup: callback must be a function")
  state.popup_observers[#state.popup_observers + 1] = fn
end

-- Configuration. Called once at boot from init.lua to set provider /
-- model / system. Idempotent for config rebinds.
function M.configure(opts)
  if type(opts) ~= "table" then return end
  if type(opts.provider) == "string" and #opts.provider > 0 then
    state.config.provider = opts.provider
  end
  if type(opts.model) == "string" and #opts.model > 0 then
    state.config.model = opts.model
  end
  if type(opts.system) == "string" and #opts.system > 0 then
    state.config.system = opts.system
  end
end

-- Inspectors / mutators used by per-plugin wrappers + resident
-- reasoners. These live on the actor so the wrapper layer stays
-- stateless.
function M.queue_sub_graph(args, gate_inner_id) return queue_sub_graph(args, gate_inner_id) end
function M.track_tool_executor(run_id, node_id, firing_id, calls, tool_ids)
  return track_tool_executor(run_id, node_id, firing_id, calls, tool_ids)
end
function M.track_provider_firing(reasoner_type, run_id, node_id, firing_id, provider_name, chat_id)
  return track_provider_firing(reasoner_type, run_id, node_id, firing_id, provider_name, chat_id)
end
function M.take_pending_for_chat(chat_id) return take_pending_for_chat(chat_id) end
function M.peek_pending_for_chat(chat_id) return peek_pending_for_chat(chat_id) end
function M.stream_visible(chat_id) return stream_visible(chat_id) end
function M.flush_pending_dispatches() return flush_pending_dispatches() end
function M.take_pending_for_tool(tool_id) return take_pending_for_tool(tool_id) end
function M.clear_pending_key(key) clear_pending_key(key) end

function M.fire_stream_observers(text) fire_stream_observers(text) end
function M.fire_reasoning_observers(text) fire_reasoning_observers(text) end
function M.fire_tool_start_observers(id, name, input) fire_tool_start_observers(id, name, input) end
function M.fire_tool_end_observers(id, output, err) fire_tool_end_observers(id, output, err) end

function M.config() return state.config end

-- Back-compat with agentic_workflow.build_template (used by tests).
function M.build_template(user_text, opts)
  opts = opts or {}
  return graph_lib.build_orchestrator_graph({
    provider  = opts.provider or state.config.provider,
    model     = opts.model    or state.config.model,
    system    = opts.system   or state.config.system,
    user_text = user_text or "",
  })
end

-- ------------------------------------------------------------------
-- receive_msg — actor runtime hook
-- ------------------------------------------------------------------

local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out entries. The broker (and ncp.lua)
  -- emit ONE entry with origin=plugin/engine and target=nil for the
  -- "logical" envelope, then N more with origin=step and target=<peer>
  -- as the fan-out copies for each ready peer. Acting on every fan-out
  -- entry would dispatch the same logical envelope N times — see
  -- e.g. reasoners.provider_run_node firing once per peer for a single
  -- reasoner-graph dispatch.
  --
  -- Sessions's actor needs the per-peer copies for replay fidelity,
  -- so we filter here (in agentic-loop's receive_msg) rather than in
  -- the actor.lua runtime.
  if entry.origin == "step" and entry.target ~= nil then return end

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

  -- Engine shutdown — sessions handles persistence; nothing for us.
  if kind == "engine.shutdown" then return end

  -- Chat-input surface from the TUI. These envelopes drive new
  -- orchestration (a `chat.input.submit` minted on resume would spawn
  -- a fresh graph the user already saw the answer for); a session
  -- replay rebuilds state via the bus markers, not by re-firing the
  -- input handlers. The `tool.result` / `graph.node.fired` block
  -- below handles the same concern for reasoner-graph emissions.
  if replay_window.active() then
    if kind == "chat.input.submit"
        or kind == "chat.reset"
        or kind == "chat.interrupt_all"
        or kind == "chat.model.set" then
      return
    end
  end

  if kind == "chat.input.submit" then handle_chat_input_submit(body); return end
  if kind == "chat.reset"        then handle_chat_reset(); return end
  if kind == "chat.interrupt_all" then cancel_all(); return end
  if kind == "chat.model.set" then handle_chat_model_set(body); return end

  -- Reasoner-graph emissions on the canonical contract:
  --   * graph.node.fired { run_id, node_id, firing_id, reasoner } —
  --     observer paired with each tool.invoke dispatch.
  --   * tool.result { id=<run_id|firing_id>, result | error } — both
  --     run-close and per-firing close share the kind; we disambiguate
  --     by id.
  if replay_window.active() then
    if kind == "graph.node.fired" then return end
    if kind == "tool.result" then
      -- Drop only tool.result envelopes that target one of our tracked
      -- ids; pass the rest to other consumers (real tool returns,
      -- spawn_graph synth replies). Matters because tool-gate's own
      -- emission goes through this same bus.
      local id = body.id
      if type(id) == "string" then
        if id == state.current_run_id
            or state.pending_runs[id] ~= nil
            or state.firing_to_node[id] ~= nil then
          return
        end
      end
    end
  end

  if kind == "graph.node.fired" then
    handle_graph_node_fired(body)
    return
  end
  if kind == "tool.result" then
    handle_tool_result(body)
    return
  end
end

-- Drive `teardown_for_session_end` from the bus marker. Replay-mode
-- gating is owned by `lib/replay_window`, which subscribes to
-- `sessions.replay.start` / `sessions.replay.end` independently — the
-- old `session_start` / `resume_done` lifecycle hooks are dead weight
-- now that the gate flips on the framing markers instead.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(_entry)
    teardown_for_session_end()
  end)
end

-- ------------------------------------------------------------------
-- module table — actor contract + public API
-- ------------------------------------------------------------------

M.name        = "agentic-loop"
M.receive_msg = receive_msg
M.send_msg    = function(_) end  -- no internal-output translation
M._internals  = {
  state = state,
  reset = function()
    state.config = { provider = "ollama", model = nil, system = nil }
    state.current_run_id = nil
    state.current_state = nil
    state.deferred_queue = {}
    state.pending_user_inputs = {}
    state.pending = {}
    state.pending_runs = {}
    state.pending_dispatches = {}
    state.chat_id_to_key = {}
    state.tool_id_to_key = {}
    state.chat_id_stream_visible = {}
    state.firing_to_node = {}
    state.stream_observers = {}
    state.reasoning_observers = {}
    state.tool_start_observers = {}
    state.tool_end_observers = {}
    state.complete_observers = {}
    state.popup_observers = {}
    envelope._reset()
  end,
}

return M
