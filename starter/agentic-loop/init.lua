-- starter/agentic-loop/init.lua — orchestrator actor.
--
-- The orchestrator owns single-flight run state, the firing→node map,
-- and the observer registries chat.lua subscribes to. Per-plugin
-- wrapper actors (under `compositors/`, plus `reasoner-graph/`,
-- `tool-gate/`) translate each binary's wire shape to the canonical
-- `tool.invoke` / `tool.result` pair; this actor only consumes the
-- canonical shape.
--
-- Inbound dispatch:
--   * `chat.input.submit { text }`       — build orchestrator graph,
--                                          emit tool.invoke{name=spawn_graph}
--   * `chat.interrupt_all`               — double-Esc; cancel everything
--   * `chat.reset`                       — /new: clear state + broadcast
--   * `chat.model.set`                   — runtime model switch
--   * `graph.run_started`/`node.fired`   — observer envelopes
--   * `tool.result { id, result|error }` — three flavours keyed by id:
--                                          orchestrator run, sub-graph
--                                          run, or wrap-node next_state
--   * `<provider>.chat.complete.result`  — translated by provider wrapper
--   * `<provider>.stream.delta` etc.     — fire stream/reasoning observers
--   * `sessions.session_start/end/resume_done` — replay lifecycle
--   * `engine.shutdown`                  — no-op (sessions handles)

local json = nefor.json

local envelope        = require("core.envelope")
local event           = require("core.event")
local ids             = require("core.ids")
local results_lib     = require("agentic-loop.results")
local topology        = require("agentic-loop.topology")
local spawn_graph     = require("libs.spawn-graph")
local history_replay  = require("core.history_replay")
local session_config  = require("agentic-loop.session_config")
local generic_provider = require("libs.generic-provider")
local generic_tool     = require("libs.generic-tool")

local state = {
  -- Orchestrator config — mutated by configure() / chat.model.set.
  config = {
    provider       = "ollama",
    model          = nil,
    reasoning_effort = nil,
    system         = nil,
    -- Optional list of tool names the lead's chat is allowed to see in
    -- its catalog. nil = no filter (advertise the full catalog). Set in
    -- starter/init.lua to lead_role.ORCHESTRATION_TOOLS so the lead
    -- can't call reasoner-graph internals like `spawn_graph` directly.
    tool_allowlist = nil,
  },

  current_run_id = nil,         ---@type string|nil
  current_state  = nil,         ---@type table|nil   -- next_state from wrap
  deferred_queue       = {},    ---@type table       -- queued spawn_graph results { text }
  pending_user_inputs  = {},    ---@type table       -- queued submits while busy
  pending              = {},    ---@type table       -- run_id:firing_id → entry
  pending_runs         = {},    ---@type table       -- sub-graph run_id → meta
  pending_dispatches   = {},    ---@type table       -- queued sub-graph dispatches
  chat_id_to_key       = {},    ---@type table
  tool_id_to_key       = {},    ---@type table
  chat_id_stream_visible = {},  ---@type table
  chat_id_stream_explicitly_hidden = {},  ---@type table  -- chats explicitly registered hidden by agent reasoners; gates streams without a pending entry
  current_lead_chat_id = nil,   ---@type string|nil
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
}

-- Reasoner types whose streaming should reach nefor-tui.
local STREAM_VISIBLE_TYPES = { ["provider-wrapper"] = true }

local SPAWN_GRAPH_SOURCE = spawn_graph.SPAWN_GRAPH_SOURCE

local emit           = envelope.emit
local pending_key    = ids.pending_key
local uuid_lite      = envelope.uuid_lite

local serialise_results = results_lib.serialise_results
local format_deferred   = results_lib.format_deferred

local function fire_observers(list, ...)
  for _, cb in ipairs(list) do pcall(cb, ...) end
end

-- Release every queued sub-graph dispatch. Idempotent.
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

local cancel_all_pending_runs

local function submit_orchestrator_run(user_text)
  if state.current_run_id ~= nil then return nil end

  local g = topology.build_orchestrator_graph({
    provider       = state.config.provider,
    model          = state.config.model,
    reasoning_effort = state.config.reasoning_effort,
    system         = state.config.system,
    user_text      = user_text or "",
    tool_allowlist = state.config.tool_allowlist,
    provider_out   = generic_provider.PROVIDER_OUT,
    final_answer   = generic_provider.FINAL_ANSWER,
    tool_calls     = generic_tool.TOOL_CALLS,
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

local function drain_deferred_text()
  if #state.deferred_queue == 0 then return nil end
  local parts = {}
  while #state.deferred_queue > 0 do
    local entry = table.remove(state.deferred_queue, 1)
    parts[#parts + 1] = entry.text
  end
  return table.concat(parts, "\n\n---\n\n")
end

local function active_lead_chat_id()
  if type(state.current_state) == "table"
      and type(state.current_state.chat_id) == "string"
      and #state.current_state.chat_id > 0 then
    return state.current_state.chat_id
  end
  if type(state.current_lead_chat_id) == "string"
      and #state.current_lead_chat_id > 0 then
    return state.current_lead_chat_id
  end
  return nil
end

local function append_to_active_lead_chat(text)
  if type(text) ~= "string" or #text == 0 then return false end
  local chat_id = active_lead_chat_id()
  if type(chat_id) ~= "string" then return false end
  emit(state.config.provider, {
    kind    = state.config.provider .. ".chat.append",
    chat_id = chat_id,
    message = { role = "user", content = text },
  })
  return true
end

-- Deferred user-text queue. Carries any text that needs to land as a
-- user-role message in a fresh orchestrator turn: today, sub-graph
-- completion bodies (`[spawn_graph(run_id=…) result]`). One entry per
-- turn so a long backlog still produces an observable chat each step
-- instead of one merged blob.
local function flush_deferred()
  if state.current_run_id ~= nil then return end
  local merged = drain_deferred_text()
  if type(merged) ~= "string" then return end
  nefor.log.info("agentic-loop: flushing deferred spawn_graph results", {
    text_preview = string.sub(merged, 1, 80),
  })
  submit_orchestrator_run(merged)
end

local function flush_pending_user_inputs()
  if state.current_run_id ~= nil then return end
  if #state.pending_user_inputs == 0 then return end
  local combined = table.concat(state.pending_user_inputs, "\n")
  nefor.log.info("agentic-loop: flushing queued user inputs", {
    count = #state.pending_user_inputs,
    text_preview = string.sub(combined, 1, 80),
  })
  state.pending_user_inputs = {}
  submit_orchestrator_run(combined)
end

-- Cancel everything. Fan-out order:
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
  local deferred_after_cancel = #state.deferred_queue
  local dropped_inputs = #state.pending_user_inputs
  if cancelled_chat then
    -- Keep the synthesized cancellation result queued. The interrupted
    -- provider turn is being torn down, so starting an automatic relay
    -- turn here would fight the user's cancellation. The next live user
    -- submit prepends this queued notice to the model prompt.
    nefor.log.info("agentic-loop: retaining interrupted sub-graph notice for next submit", {
      deferred_queued = deferred_after_cancel,
    })
  else
    flush_deferred()
  end
  state.pending_user_inputs = {}
  state.pending = {}
  state.chat_id_to_key = {}
  state.chat_id_stream_visible = {}
  state.chat_id_stream_explicitly_hidden = {}
  state.tool_id_to_key = {}
  state.firing_to_node = {}
  state.current_lead_chat_id = nil
  nefor.log.info("agentic-loop: cancel_all", {
    cancelled_chat_run = cancelled_chat,
    cancelled_sub_runs = sub_n,
    deferred_after_cancel = deferred_after_cancel,
    dropped_pending_inputs = dropped_inputs,
  })
  return {
    chat = cancelled_chat,
    sub_graphs = sub_n,
    deferred = deferred_after_cancel,
    pending_inputs = dropped_inputs,
  }
end

-- /new handler.
--
-- Clears orchestrator-side state so the next submit mints a fresh
-- chat_id under the active provider. Does NOT broadcast `chat.reset`
-- — that envelope translates to `<provider>.reset` on the wire which
-- providers handle as `reset_all()`, wiping every chat history they
-- hold (not just the active chat). With reset_all in the /new path,
-- a later /resume of any prior chat under the same provider lands on
-- a chat_id whose history the provider no longer has — the model
-- replies with no context.
--
-- The clean semantics: each chat_id is an independent conversation
-- that lives on the provider for the lifetime of the provider
-- process. /new starts a NEW chat_id (fresh state from the model's
-- perspective by virtue of no prior messages) without touching
-- siblings. /resume restores any chat_id and the provider still has
-- its history. Cross-process resume (after restarting nefor) still
-- needs an explicit replay-from-session-log — tracked separately.
local function new_chat()
  state.current_state = nil
  state.current_run_id = nil
  state.deferred_queue = {}
  state.pending_user_inputs = {}
end

-- Mid-chat /model picker.
--
-- A model-only switch (same provider, different model) keeps
-- state.current_state intact — chat_id continues to live on the
-- existing provider plugin, the wrapper's chat.model.set →
-- <prefix>.model.set carries the active chat_id so the provider
-- learns the new model for that chat, conversation continuity holds.
--
-- A provider switch (different provider name) crosses a process
-- boundary: the new provider's binary has no per-chat_id history
-- table for the active chat. Without rebuild the model would reply
-- with no memory of prior turns. We rebuild by walking the
-- on-disk session log for the prior chat's `<old>.chat.{create,append}`
-- + wrap-firing tool.result envelopes and re-emitting them as
-- `<new>.chat.{create,append}` against a fresh chat_id under the new
-- provider. The bus traffic flows through every wrapper's `to_plugin`
-- the same way live envelopes do; the new provider's wrapper delivers
-- to its binary, every other wrapper drops on prefix mismatch. The
-- next submit seeds the wrap node with the new chat_id (via
-- state.current_state) so reasoners.lua's no-create branch fires
-- (prev_state.chat_id is set → no new chat.create, just chat.append +
-- chat.complete on the already-rebuilt chat).
--
-- If the session log is unavailable (no path, open failure, no prior
-- chat.create matching the chat_id), we fall back to clearing
-- current_state — user loses prior context but the chat stays
-- consistent.
local function set_model(provider, model)
  local prior_provider = state.config.provider
  local prior_chat_id  =
    (type(state.current_state) == "table" and type(state.current_state.chat_id) == "string")
      and state.current_state.chat_id or nil
  local provider_changed = false
  if type(provider) == "string" and #provider > 0 then
    if state.config.provider ~= provider then
      provider_changed = true
    end
    state.config.provider = provider
  end
  if type(model) == "string" and #model > 0 then
    state.config.model = model
  end
  if not provider_changed then return end

  if prior_chat_id == nil then
    -- Cross-provider switch with no prior chat — nothing to rebuild.
    nefor.log.info("agentic-loop.set_model: provider changed, no prior chat", {
      prior_provider = prior_provider, new_provider = provider,
    })
    state.current_state = nil
    return
  end

  -- Rebuild prior chat under the new provider. The session log path is
  -- resolved through the sessions module; we lazy-require it to avoid
  -- a circular dep at module-load time (sessions doesn't depend on
  -- agentic-loop, but lazy keeps the surface symmetric with the
  -- agentic-loop ↔ openai-provider lazy-bind below).
  local sessions = require("sessions")
  local path = sessions.current_path()
  if type(path) ~= "string" or path == "" then
    nefor.log.warn("agentic-loop.set_model: no session log path available; clearing current_state", {
      prior_provider = prior_provider, new_provider = provider,
    })
    state.current_state = nil
    return
  end

  local target_chat_id = envelope.next_id("chat")
  local n, err = history_replay.replay_chat_history {
    path             = path,
    src_prefix       = prior_provider,
    src_chat_id      = prior_chat_id,
    target_provider  = provider,
    target_chat_id   = target_chat_id,
    model            = state.config.model,
  }
  if err ~= nil then
    nefor.log.warn("agentic-loop.set_model: history replay failed; clearing current_state", {
      prior_provider = prior_provider, new_provider = provider,
      prior_chat_id = prior_chat_id, error = err,
    })
    state.current_state = nil
    return
  end
  state.current_state = { chat_id = target_chat_id }
  nefor.log.info("agentic-loop.set_model: chat history fed to new provider", {
    prior_provider  = prior_provider, new_provider = provider,
    prior_chat_id   = prior_chat_id,  new_chat_id  = target_chat_id,
    envelopes_emitted = n,
  })
end

local function set_reasoning_effort(provider, effort)
  if type(provider) == "string" and #provider > 0 then
    state.config.provider = provider
  end
  if type(effort) ~= "string" or #effort == 0 then return end
  state.config.reasoning_effort = effort
  emit(nil, {
    kind     = "chat.reasoning.set_ack",
    provider = state.config.provider,
    effort   = effort,
  })
end

local function set_mode(mode)
  if mode == "normal" then mode = "safe" end
  if mode ~= "safe" and mode ~= "auto" and mode ~= "yolo" then return end
  emit("tool-gate", {
    kind = "tool-gate.set_mode",
    mode = mode,
  })
  nefor.log.info("agentic-loop.set_mode: tool-gate mode requested", { mode = mode })
end

local function set_yolo(enabled)
  set_mode(enabled and "yolo" or "safe")
end

-- Single-Esc behaviour: cancel the current chat turn at the provider.
-- The binary handles the history shape: mid-tool-call interrupts get
-- synthetic tool results ("tool was interrupted"), mid-stream interrupts
-- push the partial assistant text. No extra notice needed here — the
-- next user message provides natural context.
local function cancel()
  if state.current_run_id == nil then return end
  emit(state.config.provider, {
    kind = state.config.provider .. ".interrupt",
  })
end

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

  if state.current_run_id ~= nil then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = text
    return
  end

  -- Echo the user message to the TUI as a transcript-bound event so
  -- replay can repaint user turns (chat.lua dedupes against the local
  -- push, so live view sees the user line exactly once).
  emit("nefor-tui", {
    kind = "chat.message.append",
    role = "user",
    text = text,
  })

  local deferred = drain_deferred_text()
  if type(deferred) == "string" then
    text = deferred .. "\n\n---\n\n" .. text
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

local function handle_chat_reasoning_set(body)
  local effort = body.effort or body.reasoning_effort
  local provider = body.provider
  if type(effort) == "string" and #effort > 0 then
    nefor.log.info("agentic-loop: chat.reasoning.set received", {
      provider = provider, effort = effort, previous = state.config.reasoning_effort,
    })
    set_reasoning_effort(provider, effort)
  end
end

local function sub_graph_nodes(graph)
  local nodes = {}
  if type(graph) == "table" and type(graph.nodes) == "table" then
    for _, n in ipairs(graph.nodes) do
      if type(n) == "table" then
        nodes[#nodes + 1] = {
          id   = tostring(n.id or "?"),
          role = tostring(n.role or n.reasoner or "?"),
        }
      end
    end
  end
  return nodes
end

local function close_sub_graph(run_id, sub_pending, status, results, explicit_error)
  local effective_status = status or "unknown"
  local completion = {
    kind   = "spawn_graph.completed",
    run_id = run_id,
    status = effective_status,
  }
  local pending_graph = sub_pending and sub_pending.graph or nil
  if effective_status == "success" then
    completion.output = serialise_results(results or {}, pending_graph)
  elseif type(explicit_error) == "string" and #explicit_error > 0 then
    completion.error = explicit_error
  else
    completion.error = "spawn_graph run completed with status `" .. effective_status ..
                       "`: " .. json.encode(results or {})
  end

  nefor.log.info("agentic-loop: sub-graph completed", {
    run_id = run_id, status = effective_status,
  })

  local graph_event = {
    kind   = "chat.graph_result.append",
    run_id = run_id,
    status = effective_status == "success" and "success" or "failed",
    nodes  = sub_graph_nodes(pending_graph),
  }
  if effective_status == "success" then
    graph_event.output = completion.output
  else
    graph_event.error = completion.error
  end
  emit("nefor-tui", graph_event)
  state.deferred_queue[#state.deferred_queue + 1] = { text = format_deferred(completion) }
end

cancel_all_pending_runs = function()
  local n = 0
  for run_id, entry in pairs(state.pending_runs) do
    emit("reasoner-graph", { kind = "graph.cancel", run_id = run_id })
    close_sub_graph(run_id, entry, "interrupted", {},
      "sub-graph interrupted by user")
    state.pending_runs[run_id] = nil
    n = n + 1
  end
  state.pending_dispatches = {}
  return n
end

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
    close_sub_graph(run_id, sub_pending, status, results, nil)
    -- Deliver each finished graph as soon as the lead is idle. If the
    -- lead is currently processing another completion, flush_deferred()
    -- naturally leaves the text queued until that turn closes.
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
-- current_state for chat continuity. Only the wrap-firing's next_state
-- matters to the orchestrator.
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

local function teardown_for_session_end()
  if state.current_run_id ~= nil then
    emit("reasoner-graph", {
      kind = "graph.cancel",
      run_id = state.current_run_id,
    })
    emit(state.config.provider, {
      kind = state.config.provider .. ".interrupt",
    })
    state.current_run_id = nil
  end
  cancel_all_pending_runs()
  local close_notice = drain_deferred_text()
  if type(close_notice) == "string" then
    append_to_active_lead_chat(close_notice)
  end
  state.pending_runs       = {}
  state.pending_dispatches = {}
  state.pending            = {}
  state.chat_id_to_key     = {}
  state.chat_id_stream_visible = {}
  state.chat_id_stream_explicitly_hidden = {}
  state.tool_id_to_key     = {}
  state.firing_to_node     = {}
  state.current_lead_chat_id = nil
  state.current_state      = nil
  state.deferred_queue     = {}
  state.pending_user_inputs = {}
  -- Don't broadcast `chat.reset` here either — same reason as new_chat
  -- above. Provider-side chat histories stay so /resume of any prior
  -- chat under the same provider gets its history back.
  nefor.log.info("agentic-loop: sessions.session_end → state cleared", {})
end

-- Queue a sub-graph dispatch and return the minted run_id. Called by
-- the tool-gate wrapper when it intercepts the gate-forwarded
-- spawn_graph invocation. The dispatch is held in pending_dispatches
-- and released on first wrap-stream delta, or via the backup path on
-- chat.complete.result.
local function queue_sub_graph(args, gate_inner_id)
  local g = args.graph
  local on_failure = args.on_node_failure or "abort"
  if type(g) ~= "table" then
    return nil, "spawn_graph: missing or non-object `graph` argument"
  end
  local run_id = uuid_lite()
  -- Retain the submitted graph so the run-close handler can surface a
  -- per-node id+role list to the TUI's graph_result entry. The graph
  -- table is otherwise opaque to agentic-loop — reasoner-graph parses
  -- + owns scheduling — but the node list is exactly the metadata the
  -- chat surface needs to render the sub-graph result block.
  state.pending_runs[run_id] = { gate_inner_id = gate_inner_id, graph = g }
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
-- responder/wrapper reasoners.
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
  -- Capture the lead's chat_id the moment its provider firing
  -- starts. `state.current_state.chat_id` only becomes available
  -- AFTER the wrap firing closes (it's pulled from the wrap's
  -- next_state), so on turn 1 — before any wrap close has happened —
  -- cancel() / cancel_all() couldn't find a chat_id and the
  -- interrupt-notice append silently no-op'd. Tracking it here makes
  -- the chat_id known the moment streaming starts, so an immediate
  -- ESC during the very first stream still injects the notice.
  if STREAM_VISIBLE_TYPES[reasoner_type] then
    state.current_lead_chat_id = chat_id
  end
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

-- Stream-visible check by chat_id (sub-graph stream gating).
local function stream_visible(chat_id)
  return state.chat_id_stream_visible[chat_id] == true
end

-- Per-chat stream-visibility registration for chats the agentic-loop
-- doesn't itself own (e.g. agent-reasoner sub-firings). The provider
-- wrapper's gate normally requires both a pending entry AND
-- stream_visible == false to suppress; the agent reasoner's chat_id
-- has neither because it's not driven through `track_provider_firing`
-- (the agent reasoner is its own state machine and would conflict
-- with the wrapper's chat.complete.result close path). We expose a
-- separate flag table so the wrapper can ask "is this chat
-- explicitly stream-suppressed?" without a pending entry.
--
-- The wrapper's stream gate becomes:
--   stream_suppressed(chat_id) = (existing wrapper-pending gate)
--                              OR (explicitly hidden via the helper)
local function register_chat_stream_hidden(chat_id)
  if type(chat_id) ~= "string" or chat_id == "" then return end
  state.chat_id_stream_visible[chat_id] = false
  state.chat_id_stream_explicitly_hidden = state.chat_id_stream_explicitly_hidden or {}
  state.chat_id_stream_explicitly_hidden[chat_id] = true
end

local function unregister_chat_stream_hidden(chat_id)
  if type(chat_id) ~= "string" or chat_id == "" then return end
  state.chat_id_stream_visible[chat_id] = nil
  if state.chat_id_stream_explicitly_hidden ~= nil then
    state.chat_id_stream_explicitly_hidden[chat_id] = nil
  end
end

-- Single-call gate the provider wrappers use on inbound stream events.
-- True when EITHER (a) the chat has a tracked pending entry whose
-- reasoner type is not stream-visible, OR (b)
-- the chat was explicitly registered hidden by an agent reasoner.
local function stream_suppressed(chat_id)
  if type(chat_id) ~= "string" or chat_id == "" then return false end
  if state.chat_id_to_key[chat_id] ~= nil
      and state.chat_id_stream_visible[chat_id] == false then
    return true
  end
  if state.chat_id_stream_explicitly_hidden ~= nil
      and state.chat_id_stream_explicitly_hidden[chat_id] == true then
    return true
  end
  return false
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

-- Module-level exports made available to wrappers + reasoner actors
-- via `require("agentic-loop").<helper>`. Centralising the state-
-- mutation surface here keeps wrappers structurally simple (pure
-- translation; no private state) and the agentic-loop the single
-- owner of orchestrator state.

local M = {}

-- Public API (consumed by agentic_cli.lua + chat.lua).
function M.submit(text, _opts) return submit_orchestrator_run(text) end
function M.cancel()      cancel() end
function M.cancel_all()  return cancel_all() end
function M.new_chat()    new_chat() end
function M.set_model(provider, model) set_model(provider, model) end
function M.set_yolo(enabled) set_yolo(enabled) end
function M.set_mode(mode) set_mode(mode) end

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
  if type(opts.reasoning_effort) == "string" and #opts.reasoning_effort > 0 then
    state.config.reasoning_effort = opts.reasoning_effort
  end
  if type(opts.system) == "string" and #opts.system > 0 then
    state.config.system = opts.system
  end
  -- tool_allowlist: list of tool names the lead's chat catalog is
  -- restricted to (forwarded as `chat.create.tools = <names>` via the
  -- orchestrator's wrap-node args). Nil disables the filter — the lead
  -- sees the full advertised catalog. Empty table means "no tools" at
  -- all (every name filtered out); pass `nil` if you want unrestricted.
  if type(opts.tool_allowlist) == "table" then
    state.config.tool_allowlist = opts.tool_allowlist
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
function M.register_chat_stream_hidden(chat_id) register_chat_stream_hidden(chat_id) end
function M.unregister_chat_stream_hidden(chat_id) unregister_chat_stream_hidden(chat_id) end
function M.stream_suppressed(chat_id) return stream_suppressed(chat_id) end
function M.flush_pending_dispatches() return flush_pending_dispatches() end
function M.take_pending_for_tool(tool_id) return take_pending_for_tool(tool_id) end
function M.clear_pending_key(key) clear_pending_key(key) end

function M.fire_stream_observers(text) fire_stream_observers(text) end
function M.fire_reasoning_observers(text) fire_reasoning_observers(text) end
function M.fire_tool_start_observers(id, name, input) fire_tool_start_observers(id, name, input) end
function M.fire_tool_end_observers(id, output, err) fire_tool_end_observers(id, output, err) end
function M.set_reasoning_effort(provider, effort) set_reasoning_effort(provider, effort) end

function M.config() return state.config end

-- Public read-only accessor for the orchestrator's current state
-- (last-seen chat_id, anything else carried forward across firings).
-- Returns the underlying table or nil — callers must treat the result
-- as read-only; mutations leak into orchestrator state. Provider
-- compositors use this to thread the active chat_id into
-- `chat.model.set` bodies; tests use it to assert state transitions.
function M.current_state() return state.current_state end

-- Best-effort active lead chat id. This falls back to the chat_id
-- captured at provider-firing start, so UI commands issued after a
-- completed turn can still address the lead conversation.
function M.current_lead_chat_id() return active_lead_chat_id() end

-- Back-compat with agentic_workflow.build_template (used by tests).
function M.build_template(user_text, opts)
  opts = opts or {}
  return topology.build_orchestrator_graph({
    provider       = opts.provider       or state.config.provider,
    model          = opts.model          or state.config.model,
    reasoning_effort = opts.reasoning_effort or state.config.reasoning_effort,
    system         = opts.system         or state.config.system,
    user_text      = user_text or "",
    tool_allowlist = opts.tool_allowlist or state.config.tool_allowlist,
    provider_out   = generic_provider.PROVIDER_OUT,
    final_answer   = generic_provider.FINAL_ANSWER,
    tool_calls     = generic_tool.TOOL_CALLS,
  })
end

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

  local evt = event.decode(entry)
  if evt == nil then return end
  local body = evt.body
  local kind = evt.kind

  -- Engine shutdown — sessions handles persistence; nothing for us.
  if kind == "engine.shutdown" then return end

  -- Chat-input surface from the TUI. These envelopes drive new
  -- orchestration (a `chat.input.submit` minted on resume would spawn
  -- a fresh graph the user already saw the answer for); a session
  -- replay rebuilds state via the bus markers, not by re-firing the
  -- input handlers. The `tool.result` / `graph.node.fired` block
  -- below handles the same concern for reasoner-graph emissions.
  if history_replay.active() then
    if kind == "chat.input.submit"
        or kind == "chat.reset"
        or kind == "chat.interrupt_all"
        or kind == "chat.model.set"
        or kind == "chat.reasoning.set" then
      return
    end
  end

  if kind == "chat.input.submit" then handle_chat_input_submit(body); return end
  if kind == "chat.reset"        then handle_chat_reset(); return end
  if kind == "chat.interrupt_all" then cancel_all(); return end
  if kind == "chat.model.set" then handle_chat_model_set(body); return end
  if kind == "chat.reasoning.set" then handle_chat_reasoning_set(body); return end

  -- Reasoner-graph emissions on the canonical contract:
  --   * graph.node.fired { run_id, node_id, firing_id, reasoner } —
  --     observer paired with each tool.invoke dispatch.
  --   * tool.result { id=<run_id|firing_id>, result | error } — both
  --     run-close and per-firing close share the kind; we disambiguate
  --     by id.
  if history_replay.active() then
    if kind == "graph.node.fired" then return end
    if kind == "tool.result" then
      -- Cross-process /resume rebuild: capture the active chat_id from
      -- replayed wrap-firing close envelopes. Live path keys this on
      -- `firing_to_node[firing_id]` populated by `graph.node.fired`,
      -- but on a fresh-process /resume firing_to_node is empty (the
      -- run completed in the prior process). The wire signature of a
      -- wrap firing close is `result.next_state.chat_id`; run-close /
      -- terminal-close / sub-graph-synth tool.results carry
      -- `result.results` / `result.text` / `result.status` instead, so
      -- the next_state.chat_id check is the discriminator.
      --
      -- Without this, the next live submit reaches submit_orchestrator_run
      -- with state.current_state==nil → no seed_chat_id → reasoners.lua
      -- mints a fresh chat-N → openai-provider's painstakingly-rebuilt
      -- history (on the OLD chat_id) is orphaned, model replies with no
      -- memory of prior turns.
      local result = body.result
      if type(result) == "table" and type(result.next_state) == "table" then
        local cid = result.next_state.chat_id
        if type(cid) == "string" and cid ~= "" then
          state.current_state = result.next_state
        end
      end
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
      -- Wrap-firing close — silently swallow during replay (state
      -- already captured above). Without this short-circuit it falls
      -- through to handle_tool_result below, which is a no-op anyway
      -- (firing_to_node is empty), but the early return makes the
      -- intent explicit.
      if type(result) == "table" and type(result.next_state) == "table"
          and type(result.next_state.chat_id) == "string"
          and result.next_state.chat_id ~= "" then
        return
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

-- Restore `state.config.{provider,model}` from the resumed session's
-- on-disk log. Without this, /resume of a chat that was originally
-- under provider A leaves state.config.provider pointing at whatever
-- the LIVE session had switched to (e.g., the user did /model B before
-- /resume-ing) — and the next live submit dispatches the resumed
-- chat_id (restored separately by the replayed wrap-firing
-- tool.result, per 91d49ef) against provider B, which doesn't own that
-- chat. Symptom: "[Error: chat 'chat-1' not found]" on the first turn
-- after /resume.
--
-- The walk reads the log fresh on every replay start; sessions's
-- `current_path()` returns the path of the session being resumed
-- (do_resume swaps state before emitting the replay markers). The
-- helper picks the latest `chat.model.set` if the session ever saw
-- /model, otherwise falls back to the prefix + model on the latest
-- `<prefix>.chat.create`. Empty / unreadable logs leave config as-is.
--
-- The model picker UI tracks the model via `chat.model.set_ack` (gated
-- against replayed acks per e647451 — replayed acks are stale relative
-- to live state). After this restore, we emit a fresh LIVE
-- `chat.model.set_ack` so chat.lua's status bar repaints with the
-- resumed session's model. The ack must be live (not gated) because
-- it carries the post-replay truth, not a replayed envelope.
local function restore_active_model_from_session_log()
  local sessions_mod = require("sessions")
  local path = sessions_mod.current_path()
  if type(path) ~= "string" or path == "" then return end
  local active = session_config.read_active_model(path)
  local provider = active.provider
  local model    = active.model
  local reasoning_effort = active.reasoning_effort

  local changed = false
  if type(provider) == "string" and #provider > 0
      and state.config.provider ~= provider then
    state.config.provider = provider
    changed = true
  end
  if type(model) == "string" and #model > 0
      and state.config.model ~= model then
    state.config.model = model
    changed = true
  end
  if type(reasoning_effort) == "string" and #reasoning_effort > 0
      and state.config.reasoning_effort ~= reasoning_effort then
    state.config.reasoning_effort = reasoning_effort
    changed = true
  end

  if changed then
    nefor.log.info("agentic-loop: /resume restored active provider/model from session log", {
      provider = state.config.provider,
      model = state.config.model,
      reasoning_effort = state.config.reasoning_effort,
    })
    -- Surface the restored selection to chat.lua's status bar / model
    -- picker. Live ack (not a replayed envelope) so chat.lua's
    -- replay_mode gate doesn't drop it. We emit it broadcast so any
    -- observer (statusline, picker, future surfaces) picks it up.
    if type(state.config.provider) == "string" and #state.config.provider > 0
        and type(state.config.model) == "string" and #state.config.model > 0 then
      emit(nil, {
        kind     = "chat.model.set_ack",
        provider = state.config.provider,
        model    = state.config.model,
      })
    end
    if type(state.config.reasoning_effort) == "string"
        and #state.config.reasoning_effort > 0 then
      emit(nil, {
        kind     = "chat.reasoning.set_ack",
        provider = state.config.provider,
        effort   = state.config.reasoning_effort,
      })
    end
  end
end

-- Drive `teardown_for_session_end` from the bus marker. Replay-mode
-- gating is owned by `core.history_replay`, which subscribes to
-- `sessions.replay.start` / `sessions.replay.end` independently — the
-- old `session_start` / `resume_done` lifecycle hooks are dead weight
-- now that the gate flips on the framing markers instead.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(_entry)
    teardown_for_session_end()
  end)
  -- Restore active provider+model on every replay start. /resume drives
  -- the replay markers; /new fires them too with an empty log, where
  -- the helper is a no-op (no chat.create / chat.model.set to read).
  nefor.bus.on_event("sessions.replay.start", function(_entry)
    restore_active_model_from_session_log()
  end)
end

M.name        = "agentic-loop"
M.receive_msg = receive_msg
M.send_msg    = function(_) end  -- no internal-output translation
M._internals  = {
  state = state,
  reset = function()
    state.config = {
      provider = "ollama",
      model = nil,
      reasoning_effort = nil,
      system = nil,
      tool_allowlist = nil,
    }
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
    state.chat_id_stream_explicitly_hidden = {}
    state.current_lead_chat_id = nil
    state.firing_to_node = {}
    state.stream_observers = {}
    state.reasoning_observers = {}
    state.tool_start_observers = {}
    state.tool_end_observers = {}
    state.complete_observers = {}
    envelope._reset()
  end,
}

return M
