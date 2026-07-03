-- starter/agentic-loop/init.lua — the lead's turn spawner.
--
-- The lead's turn is a short-lived MAG program over a persistent chat:
-- turn-as-function, `(history, message) -> response`. Per user message this
-- actor clones the shipped turn-program (agentic-loop/lead-turn.mag,
-- compiled once via `mag.load` and cached), sets the initial `mag.Task`
-- payload to the message, overlays the live config (system prompt,
-- provider/model/reasoning effort) plus the canonical history onto the lead
-- llm actor, and submits it with `mag.execute`. The constellation runs on
-- the mag kernel — the lead's tool surface rides the tool-gate capability
-- bridge like any kernel run — the final response lands in the sink, the
-- terminal `mag.run_result` closes the turn, and the constellation dies.
--
-- What outlives turns lives HERE:
--   * canonical history — per completed turn `{ user message, final answer
--     text }` is appended (intra-turn tool exchanges die with the
--     turn-program) and seeded into the next turn's llm via
--     `params.history`. `agentic_loop.turn_recorded` markers on the bus
--     make the history rebuildable on /resume.
--   * queueing/orchestration — queued-message promotion while busy, the
--     deferred relay queue for dispatched-run completions
--     (lead-workflow → relay_run_completion), model/profile switching, the
--     statusline runtime states.
--
-- Transcript binding: kernel wire ids are run-scoped (`r<K>/…`), so per
-- run this actor broadcasts `chat.lead.bound { chat_prefix }` — the scope
-- token off `mag.run_started` plus the lead llm's actor id — and the chat
-- surface renders exactly the chats under that prefix. The same prefix
-- keys the lead's gated tool invocations (`<scope>/cap-N`) into
-- `chat.tool.start` / `chat.tool.end` transcript events.
--
-- Interrupt = kill: Esc (`chat.interrupt` / `chat.interrupt_all`) kills the
-- active run via `mag.kill_run`; the kernel reaps the constellation through
-- the fold (provider cancels fire) and settles the turn as
-- `mag.run_result status:"killed"` — turn aborted, no history append.
--
-- The pending/chat_id/tool-id tracking tables below serve the resident
-- reasoners + provider/tool compositors for reasoner-graph runs (non-lead
-- consumers — the agent reasoner's sub-firings and any directly submitted
-- graph). The lead no longer compiles onto reasoner-graph.
--
-- Inbound dispatch:
--   * `chat.input.submit { text }`       — spawn a turn-program (or queue)
--   * `chat.interrupt`                   — Esc; kill the active lead run
--   * `chat.interrupt_all`               — double-Esc; kill + drop queues
--   * `chat.reset`                       — /new: clear state + history
--   * `chat.model.set`                   — runtime model switch
--   * `mag.loaded` / `mag.error`         — the turn-program load handshake
--   * `mag.run_started { run_id, scope }`— bind the transcript prefix
--   * `mag.run_result { run_id, status }`— close the turn
--   * `<gate>.tool.invoke` / `tool.result` (lead-scoped ids) — transcript
--     tool events + observers
--   * `agentic_loop.turn_recorded`       — replay: rebuild history
--   * `sessions.session_end`             — teardown

local json = nefor.json

local envelope        = require("core.envelope")
local ids             = require("core.ids")
local results_lib     = require("agentic-loop.results")
local history_replay  = require("core.history_replay")
local session_config  = require("agentic-loop.session_config")

local state = {
  -- Orchestrator config — mutated by configure() / chat.model.set.
  config = {
    provider         = "ollama",
    model            = nil,
    reasoning_effort = nil,
    system           = nil,
  },

  -- The shipped turn-program. `source_dir`/`entry` are composition-owned
  -- (configure { lead_program = … }); the modification is loaded once per
  -- session through the mag plugin and cached here, then cloned per turn.
  lead_program = {
    source_dir = nil,   ---@type string|nil  resolved lazily (NEFOR_CONFIG_DIR)
    entry      = "agentic-loop/lead-turn.mag",
    modification = nil, ---@type table|nil   cached compiled modification
    hash       = nil,   ---@type string|nil
    entry_actor = nil,  ---@type string|nil  the task message's target
    llm_actor  = nil,   ---@type string|nil  the overlay/binding target
    load_id    = nil,   ---@type string|nil  in-flight mag.load request id
  },

  -- Canonical conversation history: provider-dialect messages, one
  -- user+assistant pair per completed turn. Seeded into each turn's llm
  -- via params.history; rebuilt on /resume from turn_recorded markers.
  history = {},                 ---@type table

  current_run_id = nil,         ---@type string|nil
  -- The in-flight turn: { run_id, user_text, scope, chat_prefix, streamed }.
  current_turn   = nil,         ---@type table|nil
  deferred_queue       = {},    ---@type table  queued relay texts { text }
  pending_user_inputs  = {},    ---@type table  queued submits while busy

  -- reasoner-graph firing bookkeeping (non-lead consumers: resident
  -- reasoners + compositors).
  pending              = {},    ---@type table  run_id:firing_id → entry
  chat_id_to_key       = {},    ---@type table
  tool_id_to_key       = {},    ---@type table
  chat_id_stream_visible = {},  ---@type table
  chat_id_stream_explicitly_hidden = {},  ---@type table
  current_lead_chat_id = nil,   ---@type string|nil

  -- Observer registries. Public on_* setters append; producers fire via
  -- pcall so a bad observer doesn't break the chain.
  stream_observers       = {},  ---@type table
  reasoning_observers    = {},  ---@type table
  tool_start_observers   = {},  ---@type table
  tool_end_observers     = {},  ---@type table
  complete_observers     = {},  ---@type table
}

-- Reasoner types whose streaming should reach nefor-tui (reasoner-graph
-- runs; the lead's kernel chats are prefix-bound instead).
local STREAM_VISIBLE_TYPES = { ["provider-wrapper"] = true }

local emit           = envelope.emit
local pending_key    = ids.pending_key

local format_deferred = results_lib.format_deferred

local function starts_with(s, prefix)
  return type(s) == "string" and type(prefix) == "string" and #prefix > 0
    and s:sub(1, #prefix) == prefix
end

local function emit_runtime_state(kind, extra)
  extra = extra or {}
  extra.kind = kind
  emit(nil, extra)
end

local function emit_idle_state(reason, run_id)
  emit_runtime_state("agentic_loop.runtime_state", {
    state  = "idle",
    reason = reason,
    run_id = run_id,
  })
end

local function emit_idle_if_idle(run_id)
  if state.current_run_id ~= nil then return end
  if #state.deferred_queue > 0 then return end
  if #state.pending_user_inputs > 0 then return end
  emit_runtime_state("agentic_loop.idle", { run_id = run_id })
end

local function fire_observers(list, ...)
  for _, cb in ipairs(list) do pcall(cb, ...) end
end

-- ── the turn-program ──────────────────────────────────────────────────

local function lead_program_source_dir()
  if type(state.lead_program.source_dir) == "string"
      and #state.lead_program.source_dir > 0 then
    return state.lead_program.source_dir
  end
  return rawget(_G, "NEFOR_CONFIG_DIR") or os.getenv("NEFOR_CONFIG_DIR") or "."
end

-- Deep-copy plain data (the cached modification / history are cloned per
-- turn so per-turn mutation never leaks into the cache).
local function deep_clone(value)
  if type(value) ~= "table" then return value end
  local out = {}
  for k, v in pairs(value) do out[k] = deep_clone(v) end
  return out
end

-- Derive the turn-program's seams from its compiled modification:
--   * entry actor — the initial task message's target;
--   * lead llm — the llm-factory actor the entry adapter routes
--     `generic-provider.ProviderOut` into (the overlay + binding target).
-- Derivation over hardcoding keeps the program hackable: rename the agent
-- in lead-turn.mag and the spawner follows.
local function derive_program_seams(modification)
  local msg = (modification.messages or {})[1]
  local entry_actor = type(msg) == "table" and msg.to or nil
  if type(entry_actor) ~= "string" then
    return nil, "turn-program has no initial message (no entry actor)"
  end
  local llm_actor
  for _, actor in ipairs(modification.actors or {}) do
    if actor.id == entry_actor then
      local dests = type(actor.routes) == "table"
        and actor.routes["generic-provider.ProviderOut"] or nil
      llm_actor = type(dests) == "table" and dests[1] or nil
    end
  end
  if type(llm_actor) ~= "string" then
    return nil, "turn-program entry actor '" .. tostring(entry_actor)
      .. "' routes no ProviderOut (no lead llm actor)"
  end
  return { entry_actor = entry_actor, llm_actor = llm_actor }, nil
end

-- Kick the turn-program load handshake (idempotent while in flight). The
-- mag.loaded reply caches the modification and flushes queued submits.
local function ensure_lead_program_loaded()
  local p = state.lead_program
  if p.modification ~= nil or p.load_id ~= nil then return end
  p.load_id = "lead-turn-load-" .. envelope.uuid_lite()
  emit("mag", {
    kind       = "mag.load",
    id         = p.load_id,
    source_dir = lead_program_source_dir(),
    entry      = p.entry,
  })
  nefor.log.info("agentic-loop: loading lead turn-program", {
    source_dir = lead_program_source_dir(), entry = p.entry,
  })
end

local flush_pending_user_inputs

local function handle_lead_program_loaded(body)
  local p = state.lead_program
  if body.in_reply_to ~= p.load_id then return end
  p.load_id = nil
  local modification = body.modification
  if type(modification) ~= "table" then
    emit("nefor-tui", {
      kind = "chat.message.append", role = "system",
      text = "[lead turn-program load carried no modification]",
    })
    return
  end
  local seams, err = derive_program_seams(modification)
  if not seams then
    emit("nefor-tui", {
      kind = "chat.message.append", role = "system",
      text = "[lead turn-program invalid] " .. tostring(err),
    })
    return
  end
  p.modification = modification
  p.hash = body.hash
  p.entry_actor = seams.entry_actor
  p.llm_actor = seams.llm_actor
  nefor.log.info("agentic-loop: lead turn-program cached", {
    hash = p.hash, entry_actor = p.entry_actor, llm_actor = p.llm_actor,
  })
  flush_pending_user_inputs()
end

local function handle_lead_program_error(body)
  local p = state.lead_program
  if body.in_reply_to ~= p.load_id then return end
  p.load_id = nil
  emit("nefor-tui", {
    kind = "chat.message.append", role = "system",
    text = "[lead turn-program failed to compile]\n" .. tostring(body.message),
  })
  -- Queued submits stay queued; the next submit retries the load.
  emit_idle_state("lead-program-load-failed")
end

-- Spawn one turn-program for `user_text`. Clones the cached modification,
-- points the initial mag.Task at the message, overlays live config +
-- canonical history onto the lead llm actor, and submits `mag.execute`.
local function submit_orchestrator_run(user_text)
  if state.current_run_id ~= nil then return nil end
  local p = state.lead_program
  if p.modification == nil then
    -- Program not compiled yet: queue the text and (re)kick the load; the
    -- mag.loaded reply flushes the queue.
    state.pending_user_inputs[#state.pending_user_inputs + 1] = user_text
    ensure_lead_program_loaded()
    return nil
  end

  local mod = deep_clone(p.modification)
  for _, msg in ipairs(mod.messages or {}) do
    if msg.to == p.entry_actor and type(msg.content) == "table" then
      msg.content.prompt = user_text
    end
  end

  local overlay_params = {
    history = deep_clone(state.history),
  }
  if type(state.config.provider) == "string" and #state.config.provider > 0 then
    overlay_params.provider = state.config.provider
  end
  if type(state.config.model) == "string" and #state.config.model > 0 then
    overlay_params.model = state.config.model
  end
  if type(state.config.reasoning_effort) == "string" and #state.config.reasoning_effort > 0 then
    overlay_params.reasoning_effort = state.config.reasoning_effort
  end
  if type(state.config.system) == "string" and #state.config.system > 0 then
    overlay_params.system = state.config.system
  end

  local run_id = ids.mint_chat_run_id()
  state.current_run_id = run_id
  state.current_turn = {
    run_id    = run_id,
    user_text = user_text or "",
    scope     = nil,
    chat_prefix = nil,
    streamed  = false,
  }
  emit_runtime_state("agentic_loop.run_start", { run_id = run_id })

  local sessions = require("sessions")
  emit("mag", {
    kind           = "mag.execute",
    id             = run_id,
    run_id         = run_id,
    run_name       = "lead",
    session_id     = sessions.current_id(),
    modification   = mod,
    params_overlay = { [p.llm_actor] = overlay_params },
  })
  nefor.log.info("agentic-loop: lead turn submitted to mag kernel", {
    run_id = run_id,
    text_preview = string.sub(user_text or "", 1, 80),
    history_len = #state.history,
  })
  return run_id
end

local function drain_deferred_text()
  if #state.deferred_queue == 0 then return nil end
  local entry = table.remove(state.deferred_queue, 1)
  return entry and entry.text or nil
end

-- Deferred relay queue. Carries any text that needs to land as the next
-- turn's user-role task: dispatched kernel-run completion bodies relayed
-- by lead-workflow (relay_run_completion). One entry per turn so a long
-- backlog still produces an observable chat each step.
local function flush_deferred()
  if state.current_run_id ~= nil then return end
  local merged = drain_deferred_text()
  if type(merged) ~= "string" then return end
  nefor.log.info("agentic-loop: flushing deferred run completion", {
    text_preview = string.sub(merged, 1, 80),
  })
  submit_orchestrator_run(merged)
end

flush_pending_user_inputs = function()
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

-- ── interrupt = kill ──────────────────────────────────────────────────

-- Kill the active lead run via the kernel kill machinery. The kernel
-- reaps the constellation through the fold — kill handlers run, so the
-- in-flight provider request's cancel envelope reaches the bus — and
-- settles the turn as `mag.run_result status:"killed"` (handled below:
-- turn aborted, no history append). Run state clears on that reply, not
-- here, so a duplicate Esc is a kernel-side no-op.
local function kill_active_lead_run()
  if state.current_run_id == nil then return false end
  emit("mag", { kind = "mag.kill_run", run_id = state.current_run_id })
  nefor.log.info("agentic-loop: kill requested for active lead run", {
    run_id = state.current_run_id,
  })
  return true
end

-- Single-Esc: abort the current turn.
local function cancel()
  kill_active_lead_run()
end

-- Double-Esc / interrupt-all: abort the current turn and drop everything
-- queued behind it. The deferred relay queue is kept (matching the prior
-- behaviour): a dispatched run's completion still reaches the model on
-- the next submit.
local function cancel_all()
  local killed = kill_active_lead_run()
  local dropped_inputs = #state.pending_user_inputs
  state.pending_user_inputs = {}
  state.pending = {}
  state.chat_id_to_key = {}
  state.chat_id_stream_visible = {}
  state.chat_id_stream_explicitly_hidden = {}
  state.tool_id_to_key = {}
  state.current_lead_chat_id = nil
  emit_idle_state("cancelled")
  nefor.log.info("agentic-loop: cancel_all", {
    killed_lead_run = killed,
    deferred_queued = #state.deferred_queue,
    dropped_pending_inputs = dropped_inputs,
  })
  return {
    chat = killed,
    deferred = #state.deferred_queue,
    pending_inputs = dropped_inputs,
  }
end

-- /new handler: clear turn + queue + canonical history so the next submit
-- starts a fresh conversation. The turn-program cache survives — the
-- program is per-session config, not per-conversation state.
local function new_chat()
  state.current_run_id = nil
  state.current_turn = nil
  state.deferred_queue = {}
  state.pending_user_inputs = {}
  state.history = {}
end

-- Mid-chat /model picker. History is canonical here and seeded per turn
-- via params.history, so BOTH a model-only switch and a cross-provider
-- switch keep full conversation continuity with no provider-side rebuild:
-- the next turn replays the same history against the new provider/model.
local function set_model(provider, model)
  if type(provider) == "string" and #provider > 0 then
    state.config.provider = provider
  end
  if type(model) == "string" and #model > 0 then
    state.config.model = model
  end
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
    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "user",
      text = text,
    })
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
  nefor.log.info("agentic-loop: chat.reset received, clearing turn state", {
    dropped_deferred = #state.deferred_queue,
    dropped_pending_inputs = #state.pending_user_inputs,
    had_run = state.current_run_id ~= nil,
    history_len = #state.history,
  })
  new_chat()
  emit_idle_state("reset")
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

-- ── turn lifecycle (kernel events) ────────────────────────────────────

-- The lead run began: bind the transcript to its scoped wire ids. The
-- kernel's chat handles are `<scope>/<actor>@r<seq>` and change every
-- round, so the binding is prefix-form — `<scope>/<llm actor>@` — and the
-- chat surface's foreign-chat guard honors it for exactly this run.
local function handle_mag_run_started(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  if type(body.scope) ~= "string" or #body.scope == 0 then
    nefor.log.warn("agentic-loop: mag.run_started carried no scope; transcript binding skipped", {
      run_id = body.run_id,
    })
    return
  end
  turn.scope = body.scope
  turn.chat_prefix = body.scope .. "/" .. state.lead_program.llm_actor .. "@"
  emit(nil, { kind = "chat.lead.bound", chat_prefix = turn.chat_prefix })
end

-- The relay text for a run result's inline `result` (the sink's final
-- answer riding mag.run_result): its text when it carries one.
local function mag_result_text(result)
  if type(result) ~= "table" then return nil end
  if type(result.text) == "string" and #result.text > 0 then
    return result.text
  end
  if type(result.final_answer) == "string" and #result.final_answer > 0 then
    return result.final_answer
  end
  local ok, encoded = pcall(json.encode, result)
  if ok and type(encoded) == "string" then return encoded end
  return nil
end

-- Terminal close of the lead's turn-program.
--   completed — the answer already painted into the transcript via the
--     prefix-bound stream (exactly how the lead's final answer renders
--     today); when no stream flowed (a non-streaming provider), the text
--     is appended so the turn is never silently empty. Canonical history
--     gains the `{ user, answer }` pair, and a turn_recorded marker rides
--     the bus so /resume can rebuild it.
--   failed — surfaced in chat as a system line (no silent nothing).
--   killed — turn aborted (Esc): no history append, no transcript append.
local function handle_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  local run_id = turn.run_id
  state.current_run_id = nil
  state.current_turn = nil

  if body.status == "completed" then
    local answer = mag_result_text(body.result) or ""
    state.history[#state.history + 1] = { role = "user", content = turn.user_text }
    state.history[#state.history + 1] = { role = "assistant", content = answer }
    -- Durable marker: the canonical history delta for this turn. Replayed
    -- on /resume to rebuild state.history (receive_msg below).
    emit(nil, {
      kind   = "agentic_loop.turn_recorded",
      run_id = run_id,
      user   = turn.user_text,
      answer = answer,
    })
    if not turn.streamed and #answer > 0 then
      emit("nefor-tui", {
        kind = "chat.message.append",
        role = "assistant",
        text = answer,
      })
    end
    nefor.log.info("agentic-loop: lead turn completed", {
      run_id = run_id, streamed = turn.streamed,
      answer_len = #answer, history_len = #state.history,
    })
    fire_observers(state.complete_observers, run_id, "success")
    flush_deferred()
    flush_pending_user_inputs()
    emit_idle_if_idle(run_id)
    return
  end

  if body.status == "killed" then
    nefor.log.info("agentic-loop: lead turn killed", { run_id = run_id })
    fire_observers(state.complete_observers, run_id, "killed")
    if #state.pending_user_inputs > 0 then
      flush_pending_user_inputs()
    else
      emit_idle_state("cancelled", run_id)
    end
    return
  end

  -- failed (and anything else terminal we don't recognize).
  local err_text = "[lead turn failed] " .. tostring(body.error or body.status or "unknown error")
  emit("nefor-tui", {
    kind = "chat.message.append",
    role = "system",
    text = err_text,
  })
  nefor.log.warn("agentic-loop: lead turn failed", {
    run_id = run_id, error = body.error, status = body.status,
  })
  fire_observers(state.complete_observers, run_id, tostring(body.status))
  flush_deferred()
  flush_pending_user_inputs()
  emit_idle_if_idle(run_id)
end

-- Lead-scoped gated tool invocations → transcript tool events + the
-- on_tool_* observer registries. The kernel mints run-scoped correlation
-- ids (`<scope>/cap-N`); the bridge keeps them as the gate's outer id and
-- the gate echoes them on its broadcast tool.result, so a prefix match on
-- the current turn's scope identifies exactly the lead's own calls
-- (dispatched sub-runs carry their own scopes). Provider-class invokes
-- never reach the gate (the bridge drives them as chat.* conversations),
-- so this observes real tools only.
local function lead_scoped_id(body_id)
  local turn = state.current_turn
  if turn == nil or type(turn.scope) ~= "string" then return false end
  return starts_with(body_id, turn.scope .. "/")
end

local function handle_gate_invoke(body)
  if not lead_scoped_id(body.id) then return end
  fire_observers(state.tool_start_observers, body.id, body.name, body.args)
  emit("nefor-tui", {
    kind  = "chat.tool.start",
    id    = body.id,
    name  = body.name,
    input = body.args,
  })
end

local function handle_gate_result(body)
  if not lead_scoped_id(body.id) then return end
  local output = body.output
  if type(output) == "table" then
    local ok, encoded = pcall(json.encode, output)
    output = ok and encoded or "(object)"
  end
  local err = body.error ~= nil
  if err then output = tostring(body.error) end
  fire_observers(state.tool_end_observers, body.id, output, err)
  emit("nefor-tui", {
    kind   = "chat.tool.end",
    id     = body.id,
    output = output,
    error  = err,
  })
end

-- Track whether the lead's answer painted via the stream this turn (the
-- chat surface finalizes the assistant entry off `chat.stream.end`); when
-- it did, the run-result close must not append the text a second time.
local function handle_stream_marker(body)
  local turn = state.current_turn
  if turn == nil or type(turn.chat_prefix) ~= "string" then return end
  if not starts_with(body.chat_id, turn.chat_prefix) then return end
  local text = body.text or body.delta
  if type(text) == "string" and #text > 0 then
    turn.streamed = true
  end
end

local function teardown_for_session_end()
  kill_active_lead_run()
  state.current_run_id = nil
  state.current_turn   = nil
  state.pending            = {}
  state.chat_id_to_key     = {}
  state.chat_id_stream_visible = {}
  state.chat_id_stream_explicitly_hidden = {}
  state.tool_id_to_key     = {}
  state.current_lead_chat_id = nil
  state.deferred_queue     = {}
  state.pending_user_inputs = {}
  state.history            = {}
  emit_idle_state("session-ended")
  nefor.log.info("agentic-loop: sessions.session_end → state cleared", {})
end

-- ── reasoner-graph firing bookkeeping (non-lead consumers) ────────────

-- Tool-executor pending entry constructor — called by the tool-executor
-- resident reasoner when it dispatches per-call invocations and needs to
-- correlate results back to its node firing.
local function track_tool_executor(run_id, run_name, node_id, firing_id, calls, tool_ids)
  if tool_ids == nil and type(firing_id) == "table" and type(calls) == "table" then
    tool_ids = calls
    calls = firing_id
    firing_id = node_id
    node_id = run_name
    run_name = nil
  end
  local key = pending_key(run_id, firing_id)
  state.pending[key] = {
    type          = "tool-executor",
    run_id        = run_id,
    run_name      = run_name,
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
local function track_provider_firing(reasoner_type, run_id, run_name, node_id, firing_id,
                                     provider_name, chat_id)
  if chat_id == nil then
    chat_id = provider_name
    provider_name = firing_id
    firing_id = node_id
    node_id = run_name
    run_name = nil
  end
  local key = pending_key(run_id, firing_id)
  state.pending[key] = {
    type          = reasoner_type,
    run_id        = run_id,
    run_name      = run_name,
    node_id       = node_id,
    firing_id     = firing_id,
    reasoner      = reasoner_type,
    provider_name = provider_name,
    chat_id       = chat_id,
  }
  state.chat_id_to_key[chat_id] = key
  state.chat_id_stream_visible[chat_id] = STREAM_VISIBLE_TYPES[reasoner_type] == true
  -- Announce the binding so surfaces can positively identify a
  -- stream-visible provider-wrapper conversation (reasoner-graph runs;
  -- exact-match form). The lead's own turns bind prefix-form off
  -- mag.run_started instead.
  if STREAM_VISIBLE_TYPES[reasoner_type] then
    if state.current_lead_chat_id ~= chat_id then
      emit(nil, { kind = "chat.lead.bound", chat_id = chat_id })
    end
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

-- Stream-visible check by chat_id. True for tracked stream-visible
-- reasoner-graph firings AND for the active lead turn's prefix-scoped
-- kernel chats (so the provider compositor fires the public stream
-- observers for the lead's own deltas — the CLI surface reads those).
local function stream_visible(chat_id)
  if state.chat_id_stream_visible[chat_id] == true then return true end
  local turn = state.current_turn
  if turn ~= nil and type(turn.chat_prefix) == "string"
      and starts_with(chat_id, turn.chat_prefix) then
    return true
  end
  return false
end

-- Per-chat stream-visibility registration for chats the agentic-loop
-- doesn't itself own (e.g. agent-reasoner sub-firings).
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

-- ── public API ────────────────────────────────────────────────────────

local M = {}

-- Public API (consumed by cli/init.lua + chat surfaces).
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
-- model / system / turn-program location. Idempotent for config rebinds.
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
  -- lead_program: where the shipped turn-program lives. `source_dir`
  -- defaults to the config dir (NEFOR_CONFIG_DIR); compositions whose
  -- config dir is not the starter (cli-config) pass it explicitly.
  if type(opts.lead_program) == "table" then
    if type(opts.lead_program.source_dir) == "string" and #opts.lead_program.source_dir > 0 then
      state.lead_program.source_dir = opts.lead_program.source_dir
    end
    if type(opts.lead_program.entry) == "string" and #opts.lead_program.entry > 0 then
      state.lead_program.entry = opts.lead_program.entry
    end
  end
end

-- Relay a completed dispatched run to the model as a fresh turn. The
-- completion is formatted into a user-role task (format_deferred) and
-- submitted as a new turn-program once the lead is idle (deferred_queue +
-- flush_deferred). lead-workflow drives this for kernel runs the lead
-- dispatched via its `mag` tool.
-- `completion` shape: { run_id, status = "success"|"failed", output|error }.
function M.relay_run_completion(completion)
  if type(completion) ~= "table" then return end
  state.deferred_queue[#state.deferred_queue + 1] = { text = format_deferred(completion) }
  flush_deferred()
end
function M.track_tool_executor(run_id, run_name, node_id, firing_id, calls, tool_ids)
  return track_tool_executor(run_id, run_name, node_id, firing_id, calls, tool_ids)
end
function M.track_provider_firing(reasoner_type, run_id, run_name, node_id, firing_id, provider_name, chat_id)
  return track_provider_firing(reasoner_type, run_id, run_name, node_id, firing_id, provider_name, chat_id)
end
function M.take_pending_for_chat(chat_id) return take_pending_for_chat(chat_id) end
function M.peek_pending_for_chat(chat_id) return peek_pending_for_chat(chat_id) end
function M.stream_visible(chat_id) return stream_visible(chat_id) end
function M.register_chat_stream_hidden(chat_id) register_chat_stream_hidden(chat_id) end
function M.unregister_chat_stream_hidden(chat_id) unregister_chat_stream_hidden(chat_id) end
function M.stream_suppressed(chat_id) return stream_suppressed(chat_id) end
function M.take_pending_for_tool(tool_id) return take_pending_for_tool(tool_id) end
function M.clear_pending_key(key) clear_pending_key(key) end

function M.fire_stream_observers(text) fire_stream_observers(text) end
function M.fire_reasoning_observers(text) fire_reasoning_observers(text) end
function M.fire_tool_start_observers(id, name, input) fire_tool_start_observers(id, name, input) end
function M.fire_tool_end_observers(id, output, err) fire_tool_end_observers(id, output, err) end
function M.set_reasoning_effort(provider, effort) set_reasoning_effort(provider, effort) end

function M._teardown_for_session_end() return teardown_for_session_end() end

function M.config() return state.config end

-- The canonical conversation history (read-only view; one user+assistant
-- pair per completed turn). Tests and surfaces read it; mutations belong
-- to the turn lifecycle only.
function M.history() return state.history end

-- Compat accessor: the reasoner-graph lead carried chat continuity in a
-- wrap-node next_state. The kernel lead has no persistent provider chat,
-- so this is always nil; the provider compositor's chat_id fallbacks
-- handle nil.
function M.current_state() return nil end

-- Best-effort active provider-wrapper chat id (reasoner-graph firings).
function M.current_lead_chat_id() return state.current_lead_chat_id end

local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out entries. The broker (and ncp.lua)
  -- emit ONE entry with origin=plugin/engine and target=nil for the
  -- "logical" envelope, then N more with origin=step and target=<peer>
  -- as the fan-out copies for each ready peer.
  if entry.origin == "step" and entry.target ~= nil then return end

  local ok, decoded = pcall(json.decode, entry.payload)
  if not ok then return end
  local body = decoded.body
  local kind = body.kind

  -- Engine shutdown — sessions handles persistence; nothing for us.
  if kind == "engine.shutdown" then return end

  if history_replay.active() then
    -- Replay rebuilds state from markers; input handlers must not
    -- re-fire (a replayed chat.input.submit would spawn a fresh turn
    -- the user already saw the answer for).
    if kind == "chat.input.submit"
        or kind == "chat.reset"
        or kind == "chat.interrupt"
        or kind == "chat.interrupt_all"
        or kind == "chat.model.set"
        or kind == "chat.reasoning.set" then
      return
    end
    -- Canonical-history rebuild: each completed turn logged one
    -- turn_recorded marker; replaying them restores the conversation the
    -- next turn seeds into its llm.
    if kind == "agentic_loop.turn_recorded" then
      if type(body.user) == "string" then
        state.history[#state.history + 1] = { role = "user", content = body.user }
        state.history[#state.history + 1] = { role = "assistant", content = tostring(body.answer or "") }
      end
      return
    end
    -- Everything else during replay: kernel/turn lifecycle events are
    -- keyed to the (empty) live turn state and fall through harmlessly.
    return
  end

  if kind == "chat.input.submit" then handle_chat_input_submit(body); return end
  if kind == "chat.reset"        then handle_chat_reset(); return end
  if kind == "chat.interrupt"    then cancel(); return end
  if kind == "chat.interrupt_all" then cancel_all(); return end
  if kind == "chat.model.set" then handle_chat_model_set(body); return end
  if kind == "chat.reasoning.set" then handle_chat_reasoning_set(body); return end

  -- Turn-program load handshake.
  if kind == "mag.loaded" then handle_lead_program_loaded(body); return end
  if kind == "mag.error"  then handle_lead_program_error(body); return end

  -- Turn lifecycle.
  if kind == "mag.run_started" then handle_mag_run_started(body); return end
  if kind == "mag.run_result"  then handle_mag_run_result(body); return end

  -- Lead-scoped tool + stream observation.
  if type(kind) == "string" then
    if kind:match("%.tool%.invoke$") then handle_gate_invoke(body); return end
    if kind == "tool.result" then handle_gate_result(body); return end
    if kind == "chat.stream.delta" or kind == "chat.stream.end" then
      handle_stream_marker(body)
      return
    end
  end
end

-- Restore `state.config.{provider,model}` from the resumed session's
-- on-disk log. Without this, /resume of a chat that was originally
-- under provider A leaves state.config.provider pointing at whatever
-- the LIVE session had switched to. The helper picks the latest
-- `chat.model.set` if the session ever saw /model, otherwise falls back
-- to the prefix + model on the latest `<prefix>.chat.create`.
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
-- `sessions.replay.start` / `sessions.replay.end` independently.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(_entry)
    teardown_for_session_end()
  end)
  -- Restore active provider+model, and start the canonical-history
  -- rebuild from a clean slate, on every replay start. /resume drives
  -- the replay markers; /new fires them too with an empty log, where
  -- both are no-ops.
  nefor.bus.on_event("sessions.replay.start", function(_entry)
    state.history = {}
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
    }
    state.lead_program = {
      source_dir = nil,
      entry = "agentic-loop/lead-turn.mag",
      modification = nil,
      hash = nil,
      entry_actor = nil,
      llm_actor = nil,
      load_id = nil,
    }
    state.history = {}
    state.current_run_id = nil
    state.current_turn = nil
    state.deferred_queue = {}
    state.pending_user_inputs = {}
    state.pending = {}
    state.chat_id_to_key = {}
    state.tool_id_to_key = {}
    state.chat_id_stream_visible = {}
    state.chat_id_stream_explicitly_hidden = {}
    state.current_lead_chat_id = nil
    state.stream_observers = {}
    state.reasoning_observers = {}
    state.tool_start_observers = {}
    state.tool_end_observers = {}
    state.complete_observers = {}
    envelope._reset()
  end,
}

return M
