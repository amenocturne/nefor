-- starter/chat_orchestrator.lua — chat → orchestrator template wiring
-- (per parent spec §6.1 Stage 1 bullet 9).
--
-- Translates `chat.input.submit { text }` from nefor-chat into a
-- `reasoner-graph.run` submitting the orchestrator template graph
-- (provider-wrapper + tool-executor + adapter cycle). Persists the
-- wrapper node's `next_state` (an opaque `{ chat_id }` blob) across
-- chat submits so the conversation builds on itself.
--
-- ### Persistence (D-S1.3)
--
-- The chat plugin doesn't change. State persistence is entirely a
-- starter-glue concern: this module holds a single `current_state`
-- variable that survives across submits within one engine session.
-- A new engine session resets it to nil (=> wrapper creates a fresh
-- chat). Multi-session resume is deferred (parent spec §8 non-goals).
--
-- ### When does the cycle terminate?
--
-- Per the orchestrator template:
--   * wrap fans out `tool_split` to either ToolCalls (loop continues)
--     or FinalAnswer (escape edge fires `terminal` node).
-- The graph's `terminal` node receives the FinalAnswer and the run
-- completes. We harvest the FinalAnswer text from `results.terminal`
-- and emit it as a `chat.message.append { role = assistant, text }`.

local M = {}

local json = nefor.json
local rg_adapter = require("reasoner_graph_adapter")

-- run_id of the currently-running orchestrator graph (or nil if none).
-- One in-flight chat run at a time — concurrent submits are dropped
-- (Stage 1 simplification; Stage 2 can queue or cancel).
--
-- Async spawn_graph note: the chat run completes quickly after the
-- model's transitional turn (the sub-graph runs out-of-band). Deferred
-- spawn_graph results arrive later via `spawn_graph.completed` and
-- trigger a NEW chat run with its own `current_run_id`.
local current_run_id = nil

-- next_state from the wrapper node's last firing — fed into the next
-- submit's prev_state for the wrap node.
local current_state = nil

-- Set by `attach_spawn_graph_listener`. The chat.interrupt_all handler
-- uses this to fan-out cancels to every in-flight sub-graph run.
local spawn_graph_ref = nil

-- Queue of pending deferred messages to inject when no chat run is in
-- flight. Each entry: { text = "<formatted>" }. We can't fire a fresh
-- reasoner-graph.run while another chat run owns the wrap chat — that
-- would race the provider's chat.complete on the same chat_id. So we
-- queue and flush on `graph.run_complete`.
local deferred_queue = {}

-- Fixed run-id prefix so we can distinguish chat-driven runs from
-- spawn_graph-driven runs.
local function mint_chat_run_id()
  return string.format(
    "chat-run-%d-%d",
    os.time(),
    math.random(0, 2 ^ 31 - 1)
  )
end

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

-- The orchestrator graph template. Built fresh per submit because
-- args (system prompt, model, prev_state seed) vary.
--
-- Nodes:
--   wrap (provider-wrapper, fanout: tool_split) — runs the chat,
--     emits ProviderOut, fans out by tool_split.
--   tools (tool-executor) — runs tool calls.
--   adapt (adapter) — packs ToolResults into ProviderIn.
--   terminal (no fanout) — receives the FinalAnswer escape; the run
--     completes once it fires. Implemented as a `dummy`-shaped node
--     whose handler immediately replies with the input as output.
--
-- Stage 1: terminal is a passthrough — it receives FinalAnswer and we
-- read the text from `results.terminal.output.text` once
-- `graph.run_complete` arrives.
local function build_orchestrator_graph(opts)
  opts = opts or {}
  local provider = opts.provider or "ollama"
  local model = opts.model
  -- Empty default = no system message. Set a real string via
  -- `chat_orchestrator.configure { system = ... }` when ready.
  local system = opts.system or ""
  local user_text = opts.user_text or ""

  local wrap_args = {
    provider = provider,
    prompt   = user_text,
  }
  if type(system) == "string" and #system > 0 then
    wrap_args.system = system
  end
  if type(model) == "string" and #model > 0 then
    wrap_args.model = model
  end

  return {
    nodes = {
      {
        id       = "wrap",
        reasoner = "provider-wrapper",
        args     = wrap_args,
        fanout   = {
          ["in"] = "generic-provider.ProviderOut",
          out    = {
            "generic-tool.ToolCalls",
            "generic-provider.FinalAnswer",
          },
        },
      },
      { id = "tools",    reasoner = "tool-executor", args = {} },
      { id = "adapt",    reasoner = "adapter",       args = {} },
      { id = "terminal", reasoner = "terminal",      args = {} },
    },
    edges = {
      { from = "wrap",  to = "tools",    type = "generic-tool.ToolCalls" },
      { from = "wrap",  to = "terminal", type = "generic-provider.FinalAnswer" },
      { from = "tools", to = "adapt" },
      { from = "adapt", to = "wrap" },
    },
  }
end

-- ------------------------------------------------------------------
-- public API
-- ------------------------------------------------------------------

-- Configure provider/model used by the orchestrator. Called from
-- init.lua after the providers are spawned. Declared up-front so the
-- internal helpers below close over the local (rather than a global of
-- the same name).
local config = { provider = "ollama", model = nil }

-- Submit the orchestrator template graph as a fresh chat run. Used by
-- both `chat.input.submit` (the user typed something) and the deferred
-- spawn_graph delivery path (a sub-graph completed and we have a
-- result to inject).
--
-- Returns the freshly-minted run_id, or nil if a run was already in
-- flight (caller is responsible for queueing in that case).
local function submit_orchestrator_run(user_text)
  if current_run_id ~= nil then return nil end

  local graph = build_orchestrator_graph({
    provider  = config.provider,
    model     = config.model,
    system    = config.system,
    user_text = user_text or "",
  })

  if type(current_state) == "table" and type(current_state.chat_id) == "string" then
    graph.nodes[1].args.seed_chat_id = current_state.chat_id
  end

  current_run_id = mint_chat_run_id()
  emit("reasoner-graph", {
    kind            = "reasoner-graph.run",
    run_id          = current_run_id,
    graph           = graph,
    on_node_failure = "abort",
  })
  return current_run_id
end

-- Format a deferred spawn_graph completion into a user-role message
-- the model will see and relay. Two design constraints from earlier
-- failure modes:
--
--   * Distinct framing (`[spawn_graph(run_id=...) result]`) so the
--     model doesn't mistake it for a new user request.
--   * Direct, declarative instructions. Earlier we saw qwen spiral —
--     "the result looks incomplete, maybe I should spawn another
--     graph, maybe I should fill in the missing branch myself…" —
--     because the wording left the door open for re-analysis. Tell
--     the model exactly what to do (relay verbatim or lightly format)
--     and exactly what NOT to do (re-spawn, fabricate, second-guess).
local function format_deferred(completion)
  local run_id = completion.run_id or "?"
  if completion.status == "success" then
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") result]\n" ..
           "The sub-graph you submitted earlier has finished. " ..
           "Present the output below to the user as your reply to their " ..
           "original prompt. You may lightly reformat for readability; " ..
           "do not re-spawn the graph, do not fabricate missing content, " ..
           "do not re-analyse whether the result is complete — the " ..
           "sub-graph is the source of truth.\n\n" ..
           "--- output ---\n" ..
           tostring(completion.output or "")
  else
    return "[spawn_graph(run_id=" .. tostring(run_id) .. ") FAILED]\n" ..
           "The sub-graph you submitted earlier failed. Tell the user " ..
           "the sub-graph errored and offer to retry; do not silently " ..
           "re-spawn or fabricate a result.\n\n" ..
           "--- error ---\n" ..
           tostring(completion.error or completion.status or "unknown error")
  end
end

-- Flush as many queued deferred messages as we can. v1: one per run
-- (each completion gets its own relay turn — the model sees and
-- describes them sequentially).
local function flush_deferred()
  if current_run_id ~= nil then return end
  if #deferred_queue == 0 then return end
  local entry = table.remove(deferred_queue, 1)
  nefor.log.info("chat_orchestrator: flushing deferred spawn_graph result", {
    text_preview = string.sub(entry.text, 1, 80),
    queue_remaining = #deferred_queue,
  })
  submit_orchestrator_run(entry.text)
end

function M.configure(opts)
  if type(opts) ~= "table" then return end
  if type(opts.provider) == "string" and #opts.provider > 0 then
    config.provider = opts.provider
  end
  if type(opts.model) == "string" and #opts.model > 0 then
    config.model = opts.model
  end
  if type(opts.system) == "string" and #opts.system > 0 then
    config.system = opts.system
  end
end

-- Return the orchestrator template graph for testing/inspection. Pure
-- function — does not emit anything.
function M.build_template(user_text, opts)
  opts = opts or {}
  return build_orchestrator_graph({
    provider  = opts.provider or config.provider,
    model     = opts.model    or config.model,
    system    = opts.system   or config.system,
    user_text = user_text or "",
  })
end

-- Attach to the nefor-chat spawn. Intercepts `chat.input.submit` from
-- the chat plugin and translates it to a `reasoner-graph.run` for the
-- orchestrator template. Translates `graph.run_complete` for the
-- chat-owned run back into `chat.message.append`.
function M.for_chat()
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- nefor-chat emits `chat.reset` on /new (the openai-provider
    -- responds by wiping all chat histories). The orchestrator keeps
    -- `current_state.chat_id` across submits; if we don't reset it
    -- here, the next submit seeds a chat-id whose provider-side
    -- history has just been cleared — chat.complete then sends an
    -- empty messages array and the upstream rejects with HTTP 400.
    if kind == "chat.reset" then
      nefor.log.info("chat_orchestrator: chat.reset received, clearing current_state", {
        had_state = current_state ~= nil,
        prior_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
        dropped_deferred = #deferred_queue,
      })
      current_state = nil
      -- Drop any queued deferred results — they belong to the prior
      -- chat. Late spawn_graph.completed events will also drop in the
      -- handler below since current_state is nil.
      deferred_queue = {}
      return env
    end

    -- Double-ESC fan-out: cancel the orchestrator chat run, every
    -- sub-graph run minted via spawn_graph, and drop the deferred
    -- queue. The single-ESC `chat.interrupt` only stops the current
    -- chat turn — sub-graphs would keep running and dump their
    -- results into the next user turn. interrupt_all is the
    -- nuclear option.
    if kind == "chat.interrupt_all" then
      local cancelled_chat = current_run_id ~= nil
      if cancelled_chat then
        emit("reasoner-graph", { kind = "reasoner-graph.graph.cancel", run_id = current_run_id })
        current_run_id = nil
      end
      local sub_n = 0
      if spawn_graph_ref ~= nil and type(spawn_graph_ref.cancel_all_pending_runs) == "function" then
        sub_n = spawn_graph_ref.cancel_all_pending_runs()
      end
      local dropped = #deferred_queue
      deferred_queue = {}
      nefor.log.info("chat_orchestrator: chat.interrupt_all", {
        cancelled_chat_run = cancelled_chat,
        cancelled_sub_runs = sub_n,
        dropped_deferred = dropped,
      })
      emit("nefor-chat", {
        kind = "chat.message.append",
        role = "system",
        text = string.format(
          "[interrupted: chat=%s sub-graphs=%d deferred=%d]",
          cancelled_chat and "1" or "0", sub_n, dropped),
      })
      return nil
    end

    if kind ~= "chat.input.submit" then return env end

    local text = env.body.text or ""
    if type(text) ~= "string" or #text == 0 then return nil end

    nefor.log.info("chat_orchestrator: chat.input.submit received", {
      text_len = #text,
      text_preview = string.sub(text, 1, 80),
      had_state = current_state ~= nil,
      seed_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
      busy = current_run_id ~= nil,
      deferred_queued = #deferred_queue,
    })

    -- One run at a time. If a prior run is still in flight, drop the
    -- submit and surface a system message to the user. (A queue would
    -- be a Stage 2 feature.) Note: with async spawn_graph, the
    -- "in-flight" window is just the model's response generation —
    -- not the sub-graph runtime. So this gate fires far less often
    -- than it used to.
    if current_run_id ~= nil then
      emit("nefor-chat", {
        kind = "chat.message.append",
        role = "system",
        text = "[orchestrator busy — wait for the current turn to finish]",
      })
      return nil
    end

    local run_id = submit_orchestrator_run(text)
    nefor.log.info("chat_orchestrator: emitting reasoner-graph.run", {
      run_id = run_id,
      seed_chat_id = type(current_state) == "table" and current_state.chat_id or nil,
      prompt_preview = string.sub(text, 1, 80),
    })

    -- Drop the chat.input.submit so it doesn't also reach the
    -- legacy openai-provider chat path (which we keep dormant in
    -- init.lua per task brief option (b)).
    return nil
  end

  return { from_plugin = from_plugin }
end

-- Attach to the reasoner-graph spawn (composed alongside the type
-- adapter and the spawn_graph binding). One responsibility here:
-- on `graph.run_complete` for our run, clear `current_run_id` so the
-- next submit isn't rejected as busy, and surface a
-- `chat.message.append` ONLY for failure cases. Successful runs
-- already streamed their assistant message into nefor-chat via the
-- openai-provider adapter (chat.stream.delta + stream.end); emitting
-- another append would duplicate it.
--
-- The wrap node's `next_state` (chat_id) is NOT captured here —
-- `graph.node_result` is emitted via `nefor.engine.send` from the Lua
-- reasoner-graph adapter, which writes straight to peer connections
-- and bypasses Lua transforms. Capture happens via
-- `rg_adapter.on_node_result` instead (registered in
-- `M.attach_state_capture` below).
function M.for_reasoner_graph()
  -- graph.run_complete IS emitted by reasoner-graph itself (origin =
  -- "reasoner-graph"), so from_plugin sees it on egress. Use it to
  -- clear current_run_id, surface failures, and route async
  -- spawn_graph completions back into a fresh chat turn.
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- Note: async spawn_graph delivery does NOT come through this
    -- transform. spawn_graph.lua emits `spawn_graph.completed` via
    -- `nefor.engine.send`, which bypasses `from_plugin` chains. We
    -- subscribe in-process via `spawn_graph.on_completed(...)` (see
    -- `M.attach_spawn_graph_listener` below) and route deferred
    -- results into `deferred_queue` from there.

    if kind ~= "graph.run_complete" then return env end
    local run_id = env.body.run_id
    if run_id ~= current_run_id then return env end

    nefor.log.info("chat_orchestrator: graph.run_complete for our run", {
      run_id = run_id,
      status = env.body.status,
      had_state = current_state ~= nil,
      chat_id = type(current_state) == "table" and current_state.chat_id or nil,
      deferred_queued = #deferred_queue,
    })
    current_run_id = nil

    -- Inspect the run's status. On success: nothing to emit (streaming
    -- already showed the answer). On synthetic failure (typecheck,
    -- missing_combinators, error, reasoner-not-connected): surface a
    -- system message so the user sees the failure.
    local status = env.body.status
    local results = env.body.results or {}

    if status == "success" then
      flush_deferred()
      return env
    end

    -- Failure path: collect a short error string from synthetic-failure
    -- nodes and surface it. Look for known synthetic ids first, then
    -- any node whose status is `error`.
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

    emit("nefor-chat", {
      kind = "chat.message.append",
      role = "system",
      text = err_text,
    })
    flush_deferred()
    return env
  end
  return { from_plugin = from_plugin }
end

-- Register the next_state capture observer with rg_adapter. Call once
-- at startup (after rg_adapter is loaded). The observer fires
-- in-process when rg_adapter emits a `graph.node_result` for our run,
-- before the next submit could possibly reuse `current_state`.
function M.attach_state_capture()
  rg_adapter.on_node_result(function(run_id, node_id, firing_id, _output, next_state)
    if run_id ~= current_run_id then return end
    if node_id ~= "wrap" then return end
    if type(next_state) ~= "table" then return end
    current_state = next_state
    nefor.log.info("chat_orchestrator: captured next_state from wrap", {
      run_id = run_id,
      firing_id = firing_id,
      chat_id = next_state.chat_id,
    })
  end)
end

-- Subscribe to async spawn_graph completion. Call once at startup
-- (after spawn_graph and chat_orchestrator are required). When a
-- sub-graph finishes, queue its result as a deferred user-role
-- message and flush — kicking off a fresh chat run that lets the
-- model relay the real answer to the user.
--
-- Filtering: v1 assumes all completions belong to this orchestrator
-- (the only consumer in v1). If the chat was reset since spawn
-- (current_state == nil), silently drop — injecting into a brand-new
-- chat would be confusing. Multi-chat correlation can come later
-- when a real use case arrives.
function M.attach_spawn_graph_listener(spawn_graph)
  assert(type(spawn_graph) == "table" and type(spawn_graph.on_completed) == "function",
         "attach_spawn_graph_listener: spawn_graph module missing on_completed")
  spawn_graph_ref = spawn_graph
  spawn_graph.on_completed(function(completion)
    if type(current_state) ~= "table" or type(current_state.chat_id) ~= "string" then
      nefor.log.info("chat_orchestrator: dropping spawn_graph completion (no current chat)", {
        run_id = completion.run_id,
        status = completion.status,
      })
      return
    end
    local text = format_deferred(completion)
    nefor.log.info("chat_orchestrator: queueing deferred spawn_graph result", {
      sub_run_id = completion.run_id,
      status = completion.status,
      text_len = #text,
      busy = current_run_id ~= nil,
    })
    deferred_queue[#deferred_queue + 1] = { text = text }
    flush_deferred()
  end)
end

-- Test-only reset.
function M._reset()
  current_run_id = nil
  current_state = nil
  deferred_queue = {}
end

return M
