-- lua/libs/agentic-loop/init.lua — the lead's turn spawner.
--
-- The lead's turn is a short-lived MAG program over a persistent chat:
-- turn-as-function, `(conversation, message) -> response`. Per user message this
-- actor addresses the shipped turn-program (agentic-loop/lead-turn.mag,
-- compiled once via `mag.load` and retained as immutable data), overlays the initial `mag.Task`
-- payload, live provider/model/reasoning effort, and canonical conversation
-- identity onto the lead
-- llm actor, and submits it with `mag.execute`. The constellation runs on
-- the mag kernel — the lead's tool surface rides the tool-gate capability
-- bridge like any kernel run — the final response lands in the sink, the
-- terminal `mag.run_result` closes the turn, and the constellation dies.
--
-- What outlives turns lives in conversation-manager. This actor caches only
-- its universal context projection and never folds provider or MAG facts.
-- What remains HERE is queueing/orchestration — queued-message promotion while busy, the
--     deferred relay queue for dispatched-run completions
--     (lead-workflow → relay_run_completion), model/profile switching, the
--     statusline runtime states.
--
-- Kernel wire ids remain run-scoped (`r<K>/…`). Their prefix is private
-- correlation state for this orchestrator; transcript selection belongs to
-- conversation-manager and its active-conversation projection.
--
-- Live-turn controls:
--   * `chat.steer` claims queued input and injects it at the lead LLM's next
--     provider boundary, after the current exchange.
--   * `chat.interrupt { drop_queued = true }` kills only the active lead run;
--     the TUI restores the queued text to its prompt before sending it.
--   * `chat.interrupt_all` remains the session-wide graceful-interrupt surface
--     used by commands and mode transitions.
--
-- Inbound dispatch:
--   * `chat.input.submit { text }`       — spawn a turn-program (or queue)
--   * `chat.steer`                       — inject queued input before next LLM turn
--   * `chat.interrupt`                   — kill the active lead run
--   * `chat.interrupt_all`               — graceful interrupt + drop queues
--   * `chat.reset`                       — /new: clear state + history
--   * `chat.model.set`                   — runtime model switch
--   * `chat.compaction.request`          — compact canonical model context
--   * `mag.loaded` / `mag.error`         — the turn-program load handshake
--   * `mag.run_started { run_id, scope }`— bind the transcript prefix
--   * `mag.run_result { run_id, status }`— close the turn
--   * `<gate>.tool.invoke` / `tool.result` (lead-scoped ids) — transcript
--     tool events + observers
--   * `conversation.projection.delta`    — update the disposable context cache
--   * `conversation.context.snapshot`    — restore/query universal model context
--   * `sessions.session_end`             — teardown

local json = nefor.json

local envelope        = require("core.envelope")
local ids             = require("core.ids")
local results_lib     = require("libs.agentic-loop.results")
local error_value     = require("core.error")
local replay_window   = require("core.replay_window")
local conversation_projection = require("libs.agentic-loop.conversation_projection")
local model_snapshot_data = require("libs.model-snapshot")
local mag_workspace = require("libs.mag-workspace")
local RequestLifecycle = require("libs.agentic-loop.request-lifecycle")

-- The lead program's public result is deliberately fixed to
-- Result<AgentError, TextAnswer>; exact identities keep terminal projection nominal.
local LEAD_RESULT_TYPE_ID = "sha256:aa6047bf525dee9532563264d6e0d119dd2359ae92013e228bf405f434c4b8cd"
local LEAD_RESULT_CONSTRUCTOR_IDS = {
  Ok = "sha256:e1bfb90cb2d8f7eeac511131f77680093c131eceb1eb39a50fcfb2eae73de7fd",
  Error = "sha256:1161ef7727b26a70715789ea377f6a39efb9d897536ecc775946f64477947a5a",
}

local state = {
  -- Orchestrator config — mutated by configure() / chat.model.set.
  config = {
    provider         = "ollama",
    model            = nil,
    reasoning_effort = nil,
    system           = nil,
    ambient_context  = nil,
    resolve_model_snapshot = nil,
  },
  pending_model_selection = nil, ---@type table|nil { provider, model }

  -- The shipped turn-program. `source_dir`/`entry` are composition-owned
  -- (configure { lead_program = … }); the artifact is loaded once per
  -- session through the mag plugin and retained inline as an immutable envelope.
  lead_program = {
    source_dir = nil,   ---@type string|nil  resolved lazily (NEFOR_CONFIG_DIR)
    entry      = "agentic-loop/lead-turn.mag",
    module_roots = nil, ---@type string[]|nil explicit ordered search roots
    artifact    = nil,   ---@type table|nil   retained compiled artifact
    hash       = nil,   ---@type string|nil
    inventory  = nil,   ---@type table|nil complete initial/template actor inventory
    semantic_types = nil, ---@type table|nil stable id -> compiler-declared descriptor
    entry_actor = nil,  ---@type string|nil  the task message's target
    llm_actor  = nil,   ---@type string|nil  the overlay/binding target
    load_id    = nil,   ---@type string|nil  in-flight mag.load request id
  },

  -- Read-only projection of conversation-manager's canonical recorded facts.
  -- The loop never folds its outbound append requests optimistically: the
  -- manager's recorded acknowledgement is the sole commit boundary.
  conversation = conversation_projection.new(),
  conversation_id = nil,
  pending_conversation_create = nil,
  pending_system_seed = nil,    ---@type table|nil
  pending_compaction = nil,     ---@type table|nil
  pending_context_request = nil,
  prepare_requested = false,
  ready_announced = false,
  context_error = nil,

  current_run_id = nil,         ---@type string|nil
  -- The in-flight turn: { run_id, user_text, scope }.
  current_turn   = nil,         ---@type table|nil
  deferred_queue       = {},    ---@type table  queued relay texts { text }
  pending_user_inputs  = {},    ---@type table  queued submits while busy
  pending_steer        = nil,   ---@type table|nil queued inputs awaiting MAG steer ack

  -- Observer registries. Public on_* setters append; producers fire via
  -- pcall so a bad observer doesn't break the chain.
  stream_observers       = {},  ---@type table
  reasoning_observers    = {},  ---@type table
  tool_start_observers   = {},  ---@type table
  tool_end_observers     = {},  ---@type table
  complete_observers     = {},  ---@type table

  -- The context description is composition-owned and immutable; only the
  -- writable workspace path is session-specific.
  mag_context = {
    workspace         = nil,  ---@type string|nil
    workspace_session = nil,  ---@type string|nil
  },
}

local emit           = envelope.emit

local format_deferred = results_lib.format_deferred
local request_lifecycle
local fail_open_requests

local function conversation_history()
  return state.conversation:history()
end

local function conversation_commit_pending()
  return state.pending_compaction ~= nil
      or state.pending_context_request ~= nil
end

request_lifecycle = RequestLifecycle.new({
  emit = function(body) emit(nil, body) end,
  blocked = function()
    return conversation_commit_pending() or state.pending_conversation_create ~= nil
      or state.pending_system_seed ~= nil
  end,
})

local function append_conversation_fact(fact)
  emit("conversation-manager", {
    kind = "conversation.fact.append",
    fact = fact,
  })
end

local function model_snapshot()
  local snapshot = {
    provider = state.config.provider,
    model = state.config.model,
  }
  if type(state.config.reasoning_effort) == "string"
      and state.config.reasoning_effort ~= "" then
    snapshot.reasoning_effort = state.config.reasoning_effort
  end
  return snapshot
end

local function resolve_run_model_snapshot()
  local resolver = state.config.resolve_model_snapshot
  if resolver == nil then return nil, nil end
  local resolved, snapshot = pcall(resolver)
  if not resolved then
    return nil, "model snapshot resolver failed: " .. tostring(snapshot)
  end
  local copy, snapshot_error = model_snapshot_data.copy(snapshot)
  if copy == nil then
    return nil, "invalid model snapshot: " .. tostring(snapshot_error)
  end
  return copy, nil
end

local function configuration_provenance()
  local snapshot = model_snapshot()
  snapshot.surface = "lead"
  return snapshot
end

local function ensure_conversation_id()
  local conversation_id = state.conversation_id
  if conversation_id ~= nil then return conversation_id end
  conversation_id = "conversation-" .. envelope.uuid_lite()
  local event_id = "conversation-event-" .. envelope.uuid_lite()
  state.conversation_id = conversation_id
  state.pending_conversation_create = event_id
  append_conversation_fact({
    kind = "created",
    event_id = event_id,
    conversation_id = conversation_id,
    provenance = configuration_provenance(),
  })
  return conversation_id
end

local function conversation_ready()
  return state.conversation_id ~= nil
      and state.conversation:id() == state.conversation_id
      and state.pending_conversation_create == nil
      and state.pending_system_seed == nil
end

local function loop_ready()
  if replay_window.active() or state.context_error ~= nil
      or conversation_commit_pending() then return false end
  if state.conversation_id == nil then
    return state.pending_conversation_create == nil
  end
  return conversation_ready()
end

local function emit_ready_if_ready()
  if not state.prepare_requested or state.ready_announced or not loop_ready() then return false end
  state.ready_announced = true
  emit(nil, {
    kind = "agentic_loop.ready",
    session_id = require("libs.sessions").current_id(),
    conversation_id = state.conversation_id,
  })
  return true
end

local function record_configuration()
  if not conversation_ready() then return end
  append_conversation_fact({
    kind = "provenance_updated",
    event_id = "conversation-event-" .. envelope.uuid_lite(),
    conversation_id = state.conversation_id,
    provenance = configuration_provenance(),
  })
end

local function request_context(reason)
  if not conversation_ready() then return false end
  local request_id = "conversation-context-" .. envelope.uuid_lite()
  state.ready_announced = false
  state.context_error = nil
  state.pending_context_request = { id = request_id, reason = reason }
  emit("conversation-manager", {
    kind = "conversation.context.request",
    request_id = request_id,
    conversation_id = state.conversation_id,
  })
  return true
end

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
  if conversation_commit_pending() then return end
  if #state.deferred_queue > 0 then return end
  if #state.pending_user_inputs > 0 then return end
  emit_runtime_state("agentic_loop.idle", { run_id = run_id })
end

local function fire_observers(list, ...)
  for _, cb in ipairs(list) do pcall(cb, ...) end
end

-- ── ambient MAG context ───────────────────────────────────────────────

local function mag_config_dir()
  local sd = state.lead_program.source_dir
  if type(sd) == "string" and #sd > 0 then return sd end
  return rawget(_G, "NEFOR_CONFIG_DIR") or os.getenv("NEFOR_CONFIG_DIR") or "."
end

-- The session workspace dir, resolved the way the mag tool does (seed +
-- return); falls back to the pure path when seeding can't run (no writable
-- data root, e.g. under test). Cached per session.
local function mag_workspace_dir(session_id, config_dir)
  local mc = state.mag_context
  if mc.workspace_session == session_id and type(mc.workspace) == "string" then
    return mc.workspace
  end
  local mag = require("libs.mag-workspace")
  local ws
  local ok, res = pcall(mag.init_workspace, session_id, config_dir)
  if ok and type(res) == "string" and #res > 0 then
    ws = res
  else
    ws = mag.workspace_dir(session_id)
  end
  mc.workspace = ws
  mc.workspace_session = session_id
  return ws
end

-- The full `## MAG workspace` block, or nil when there is no active session
-- to anchor the workspace dir.
local function system_with_mag_context(base)
  local context = state.config.ambient_context
  if type(context) ~= "table" or type(context.compose) ~= "function" then return base end
  local sessions = require("libs.sessions")
  local session_id = sessions.current_id()
  if type(session_id) ~= "string" or session_id == "" then return base end
  local config_dir = mag_config_dir()
  local ws = mag_workspace_dir(session_id, config_dir)
  return context:compose(base, { workspace = ws })
end

-- Seed system content once into the append-only conversation. Provider actors
-- reconstruct it through conversation-manager like every other message; neither
-- MAG execution overlays nor provider invocation commands retain another copy.
local function record_system_prompt(conversation_id)
  local base = (type(state.config.system) == "string" and #state.config.system > 0)
    and state.config.system or nil
  local system = system_with_mag_context(base)
  if type(system) ~= "string" or system == "" then return false end

  local message_id = conversation_id .. ":system"
  local completed_event_id = "conversation-event-" .. envelope.uuid_lite()
  state.pending_system_seed = {
    message_id = message_id,
    completed_event_id = completed_event_id,
  }
  append_conversation_fact({
    kind = "message_started",
    event_id = "conversation-event-" .. envelope.uuid_lite(),
    conversation_id = conversation_id,
    message_id = message_id,
    role = "system",
  })
  append_conversation_fact({
    kind = "content_chunk_appended",
    event_id = "conversation-event-" .. envelope.uuid_lite(),
    conversation_id = conversation_id,
    message_id = message_id,
    chunk = { kind = "text", data = system },
  })
  append_conversation_fact({
    kind = "message_completed",
    event_id = completed_event_id,
    conversation_id = conversation_id,
    message_id = message_id,
    completion = {},
  })
  return true
end

-- ── the turn-program ──────────────────────────────────────────────────

local function lead_program_source_dir()
  if type(state.lead_program.source_dir) == "string"
      and #state.lead_program.source_dir > 0 then
    return state.lead_program.source_dir
  end
  return rawget(_G, "NEFOR_CONFIG_DIR") or os.getenv("NEFOR_CONFIG_DIR") or "."
end

-- Resolve the ordered MAG module search path. Compositions may add their own
-- libraries around Nefor's standard library; the historical config-owned
-- root remains the exact default when no explicit roots were configured.
local function lead_program_module_roots(source_dir)
  local configured = state.lead_program.module_roots
  if configured == nil then return { source_dir .. "/mag/lib" } end
  local roots = {}
  for i, root in ipairs(configured) do roots[i] = root end
  return roots
end

-- Deep-copy JSON-shaped data where event snapshots must not share tables.
-- Decoded JSON
-- arrays carry a private mlua metatable, which is the only distinction
-- between an empty `[]` and `{}`; semantic type descriptors rely on it.
local function deep_clone(value)
  if type(value) ~= "table" then return value end
  local out = {}
  for k, v in pairs(value) do out[k] = deep_clone(v) end
  if type(nefor.json.is_array) == "function" and nefor.json.is_array(value)
      and type(nefor.json.mark_array) == "function" then
    nefor.json.mark_array(out)
  end
  return out
end

local function deep_equal(left, right)
  if type(left) ~= type(right) then return false end
  if type(left) ~= "table" then return left == right end
  for key, value in pairs(left) do
    if not deep_equal(value, right[key]) then return false end
  end
  for key in pairs(right) do
    if left[key] == nil then return false end
  end
  return true
end

-- Derive the turn-program's seams from its compiled modification:
--   * source actor — the initial Unit message's target and task-value owner;
--   * entry actor — the source value's destination;
--   * lead llm — the llm-factory actor the entry adapter routes
--     `generic-provider.ProviderOut` into (the overlay + binding target).
-- Derivation over hardcoding keeps the program hackable: rename the agent
-- in lead-turn.mag and the spawner follows.
local function derive_program_seams(modification)
  local msg = (modification.messages or {})[1]
  local source_endpoint = type(msg) == "table" and type(msg.to) == "table" and msg.to.endpoint or nil
  local source_actor = type(source_endpoint) == "table"
      and source_endpoint.constructor == "ActorEndpoint"
      and type(source_endpoint.value) == "table" and source_endpoint.value.id or nil
  if type(source_actor) ~= "string" then
    return nil, "turn-program has no initial message (no source actor)"
  end
  local entry_actor
  local llm_actor
  for _, actor in ipairs(modification.actors or {}) do
    if actor.id == source_actor
        and (type(actor.params) ~= "table" or type(actor.params.value) ~= "table") then
      return nil, "turn-program source actor has no typed task value"
    end
  end
  for _, route in ipairs(modification.routes or {}) do
    local from, to = route.from or {}, route.to or {}
    local from_endpoint, to_endpoint = from.endpoint or {}, to.endpoint or {}
    if from_endpoint.constructor == "ActorEndpoint" and from_endpoint.value.id == source_actor
        and from.wire == "nefor.graph.Value" and to_endpoint.constructor == "ActorEndpoint" then
      entry_actor = to_endpoint.value.id
      break
    end
  end
  if type(entry_actor) ~= "string" then
    return nil, "turn-program source routes no task value (no entry actor)"
  end
  for _, route in ipairs(modification.routes or {}) do
    local from, to = route.from or {}, route.to or {}
    local from_endpoint, to_endpoint = from.endpoint or {}, to.endpoint or {}
    if from_endpoint.constructor == "ActorEndpoint" and from_endpoint.value.id == entry_actor
        and from.wire == "generic-provider.ProviderOut"
        and to_endpoint.constructor == "ActorEndpoint" then
      llm_actor = to_endpoint.value.id
      break
    end
  end
  if type(llm_actor) ~= "string" then
    return nil, "turn-program entry actor '" .. tostring(entry_actor)
      .. "' routes no ProviderInput (no lead llm actor)"
  end
  return { source_actor = source_actor, entry_actor = entry_actor, llm_actor = llm_actor }, nil
end

-- Kick the turn-program load handshake (idempotent while in flight). The
-- mag.loaded reply returns the exact immutable program artifact and flushes queued submits.
local function ensure_lead_program_loaded()
  local p = state.lead_program
  if p.artifact ~= nil or p.load_id ~= nil then return end
  p.load_id = "lead-turn-load-" .. envelope.uuid_lite()
  local source_dir = lead_program_source_dir()
  local module_roots = lead_program_module_roots(source_dir)
  emit("mag", mag_workspace.compile_request(p.load_id, source_dir, p.entry,
    module_roots, p.project_build))
  nefor.log.info("agentic-loop: loading lead turn-program", {
    source_dir = source_dir, entry = p.entry,
  })
end

local flush_pending_user_inputs

local function release_lead_program()
  local p = state.lead_program
  p.artifact = nil
  p.hash = nil
  p.inventory = nil
  p.semantic_types = nil
  p.source_actor = nil
  p.entry_actor = nil
  p.llm_actor = nil
  p.load_id = nil
end

local function handle_lead_program_loaded(body)
  local p = state.lead_program
  if body.in_reply_to ~= p.load_id then return end
  p.load_id = nil
  local artifact = body.artifact
  local decoded, decode_error = mag_workspace.decode_artifact(artifact)
  if not decoded or decoded.kind ~= "program" then
    fail_open_requests({
      code = "lead_program_unavailable",
      message = tostring(decode_error or "Lead program has no executable data"),
    })
    emit("nefor-tui", {
      kind = "chat.error.append", title = "Lead program unavailable",
      message = tostring(decode_error or "mag.loaded did not carry a program envelope"),
      retryable = true,
    })
    return
  end
  local seams, err = derive_program_seams(decoded.modification)
  if not seams then
    fail_open_requests({ code = "lead_program_invalid", message = tostring(err) })
    emit("nefor-tui", {
      kind = "chat.error.append", title = "Lead program invalid",
      message = tostring(err), retryable = false,
    })
    return
  end
  p.artifact = deep_clone(artifact)
  p.hash = body.hash
  p.inventory = mag_workspace.actor_inventory(decoded)
  p.semantic_types = deep_clone(decoded.modification.types or {})
  p.source_actor = seams.source_actor
  p.entry_actor = seams.entry_actor
  p.llm_actor = seams.llm_actor
  nefor.log.info("agentic-loop: lead turn-program retained", {
    hash = p.hash, entry_actor = p.entry_actor, llm_actor = p.llm_actor,
  })
  flush_pending_user_inputs()
end

local function handle_lead_program_error(body)
  local p = state.lead_program
  if body.in_reply_to ~= p.load_id then return end
  p.load_id = nil
  emit("nefor-tui", {
    kind = "chat.error.append",
    title = "Lead program failed to compile",
    message = tostring(body.message),
    retryable = true,
  })
  fail_open_requests({ code = "lead_program_failed", message = tostring(body.message) })
  emit_idle_state("lead-program-load-failed")
end

-- Spawn one turn-program for `user_text`. Clones the retained inline artifact and
-- overlays the initial mag.Task plus live config +
-- canonical conversation identity onto the lead llm actor, and submits
-- `mag.execute`. The provider actor reads history from conversation-manager;
-- duplicating it in this persisted command would make session growth quadratic.
local function submit_orchestrator_run(user_text, submission_ids, input_cause, release_obligations)
  if state.current_run_id ~= nil then return nil end
  local conversation_id = ensure_conversation_id()
  if not conversation_ready() then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = { text = user_text, submission_ids = submission_ids or {} }
    ensure_lead_program_loaded()
    return nil
  end
  local p = state.lead_program
  if p.artifact == nil then
    -- Program not compiled yet: queue the text and (re)kick the load; the
    -- mag.loaded reply flushes the queue.
    state.pending_user_inputs[#state.pending_user_inputs + 1] = { text = user_text, submission_ids = submission_ids or {} }
    ensure_lead_program_loaded()
    return nil
  end

  local overlay_params = {
    conversation_id = conversation_id,
    submission_ids = submission_ids,
    input_cause = input_cause,
    authored_prompt = input_cause == nil and user_text or nil,
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
  local run_model_snapshot, snapshot_error = resolve_run_model_snapshot()
  if snapshot_error ~= nil then
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Lead model unavailable",
      message = snapshot_error,
      retryable = true,
    })
    fail_open_requests({ code = "model_unavailable", message = snapshot_error })
    for _, obligation in ipairs(release_obligations or {}) do
      request_lifecycle:release_all(obligation.request_ids, obligation.id)
    end
    emit_idle_state("lead-model-snapshot-unavailable")
    return nil
  end
  local run_id = ids.mint_chat_run_id()
  state.current_run_id = run_id
  state.current_turn = {
    run_id    = run_id,
    turn_id   = run_id,
    user_text = user_text or "",
    scope     = nil,
    manager_terminal = false,
    result_body = nil,
    request_ids = RequestLifecycle.copy_ids(submission_ids),
  }
  local sessions = require("libs.sessions")
  local turn_obligation = "turn:" .. run_id
  request_lifecycle:acquire_all(state.current_turn.request_ids, turn_obligation,
    sessions.current_id())
  for _, obligation in ipairs(release_obligations or {}) do
    request_lifecycle:release_all(obligation.request_ids, obligation.id)
  end
  emit_runtime_state("agentic_loop.run_start", { run_id = run_id })

  local execute = {
    kind           = "mag.execute",
    id             = run_id,
    run_id         = run_id,
    turn_id        = run_id,
    run_name       = "lead",
    conversation_id = conversation_id,
    session_id     = sessions.current_id(),
    principal      = "lead",
    artifact       = p.artifact,
    params_overlay = {},
  }
  execute.params_overlay[mag_workspace.initial_actor_address(p.source_actor)] = {
    value = { prompt = user_text },
  }
  for _, entry in ipairs(p.inventory or {}) do
    local factory = entry.actor and entry.actor.factory
    if factory == "nefor.factory.llm" or factory == "nefor.factory.structured-output" then
      execute.params_overlay[entry.address] = deep_clone(overlay_params)
    end
  end
  if run_model_snapshot ~= nil then execute.model_snapshot = run_model_snapshot end
  envelope.emit_as("agentic-loop", "mag", execute)
  nefor.log.info("agentic-loop: lead turn submitted to mag kernel", {
    run_id = run_id,
    text_preview = string.sub(user_text or "", 1, 80),
    history_len = #conversation_history(),
  })
  return run_id
end

-- Drain the WHOLE deferred queue into one text: every run completion that
-- arrived while the lead was busy rides a single relay turn (separated so
-- each block stays readable) instead of costing one provider turn each — a
-- burst of detached eval completions would otherwise replay the full history
-- once per result.
local function drain_deferred_text()
  if #state.deferred_queue == 0 then return nil end
  local parts, request_ids, obligations = {}, {}, {}
  for _, entry in ipairs(state.deferred_queue) do
    if type(entry.text) == "string" and #entry.text > 0 then
      parts[#parts + 1] = entry.text
    end
    for _, request_id in ipairs(entry.request_ids or {}) do
      request_ids[#request_ids + 1] = request_id
    end
    if entry.obligation_id ~= nil then
      obligations[#obligations + 1] = {
        id = entry.obligation_id,
        request_ids = entry.request_ids or {},
      }
    end
  end
  state.deferred_queue = {}
  if #parts == 0 then return nil end
  return table.concat(parts, "\n\n---\n\n"),
    RequestLifecycle.copy_ids(request_ids), obligations
end

-- Deferred relay queue. Carries any text that needs to land as the next
-- turn's user-role task: dispatched kernel-run completion bodies relayed
-- by lead-workflow (relay_run_completion).
local function flush_deferred()
  if state.current_run_id ~= nil then return end
  if conversation_commit_pending() or not conversation_ready() then return end
  local merged, request_ids, obligations = drain_deferred_text()
  if type(merged) ~= "string" then return end
  nefor.log.info("agentic-loop: flushing deferred run completions", {
    text_preview = string.sub(merged, 1, 80),
  })
  submit_orchestrator_run(merged, request_ids, "internal_async_completion", obligations)
end

flush_pending_user_inputs = function()
  if state.current_run_id ~= nil then return end
  if conversation_commit_pending() or not conversation_ready() then return end
  if state.lead_program.artifact == nil then return end
  if #state.pending_user_inputs == 0 then return end
  local inputs = state.pending_user_inputs
  local texts, submission_ids = {}, {}
  for _, input in ipairs(inputs) do
    texts[#texts + 1] = input.text
    for _, id in ipairs(input.submission_ids or {}) do submission_ids[#submission_ids + 1] = id end
  end
  local combined = table.concat(texts, "\n")
  nefor.log.info("agentic-loop: flushing queued user inputs", {
    count = #inputs,
    text_preview = string.sub(combined, 1, 80),
  })
  state.pending_user_inputs = {}
  emit("nefor-tui", { kind = "chat.queue.steered" })
  local obligations = {}
  for _, request_id in ipairs(RequestLifecycle.copy_ids(submission_ids)) do
    obligations[#obligations + 1] = {
      id = "input:" .. request_id,
      request_ids = { request_id },
    }
  end
  submit_orchestrator_run(combined, submission_ids, nil, obligations)
end

-- ── interrupt = kill ──────────────────────────────────────────────────

-- Kill the active lead run via the kernel kill machinery. The kernel
-- reaps the constellation through the fold — kill handlers run, so the
-- in-flight provider request's cancel envelope reaches the bus — and
-- settles the turn as `mag.run_result status:"killed"` (handled below:
-- turn interrupted). Run state clears after both that reply and the manager's
-- terminal turn projection, not
-- here, so a duplicate Esc is a kernel-side no-op.
local function kill_active_lead_run()
  if state.current_run_id == nil then return false end
  emit("mag", { kind = "mag.kill_run", run_id = state.current_run_id })
  nefor.log.info("agentic-loop: kill requested for active lead run", {
    run_id = state.current_run_id,
  })
  return true
end

-- Graceful interrupt of the active lead run (NOT a kill). The kernel settles
-- whatever capability the run is blocked on as a
-- failed "interrupted by user" result and cancels the real work (a bash
-- subprocess dies, a provider round aborts); the lead re-fires with that
-- failure in context and winds the turn down with a real final answer. The run
-- SURVIVES — `current_run_id` stays set — so the turn closes through the normal
-- `mag.run_result status:"completed"` path. The manager's terminal projection
-- is the commit boundary, so there is no completed-but-unrecorded promotion.
local function interrupt_active_lead_run()
  if state.current_run_id == nil then
    -- The lead is idle: it dispatched fire-and-forget sub-runs (the `mag`
    -- execute tool acks "executing" and the turn completes) and is no longer
    -- blocked on anything this entry point can see. Those detached runs are
    -- interrupted by lead-workflow's own `chat.interrupt_all` subscription
    -- (it owns state.active_runs). Log so the next live repro is diagnosable
    -- without a full trace — an interrupt landing here is expected whenever the
    -- churning work is detached rather than a direct lead tool call.
    nefor.log.warn(
      "agentic-loop: interrupt with no active lead run — lead is idle; " ..
      "detached dispatched runs are interrupted by lead-workflow", {})
    return false
  end
  emit("mag", { kind = "mag.interrupt_run", run_id = state.current_run_id })
  nefor.log.info("agentic-loop: graceful interrupt requested for active lead run", {
    run_id = state.current_run_id,
  })
  return true
end

-- Abort the current lead turn. When requested by the TUI's hard-stop paths,
-- queued input has already been restored to the prompt and must not respawn.
local function cancel(drop_queued)
  if drop_queued then
    state.pending_user_inputs = {}
    state.pending_steer = nil
  end
  kill_active_lead_run()
end

local function steer_pending_inputs()
  if state.current_run_id == nil or state.pending_steer ~= nil then return false end
  if #state.pending_user_inputs == 0 then return false end
  local inputs = state.pending_user_inputs
  state.pending_user_inputs = {}
  local texts, submission_ids = {}, {}
  for _, input in ipairs(inputs) do
    texts[#texts + 1] = input.text
    for _, id in ipairs(input.submission_ids or {}) do submission_ids[#submission_ids + 1] = id end
  end
  local text = table.concat(texts, "\n")
  local id = "lead-steer-" .. envelope.uuid_lite()
  state.pending_steer = {
    id = id,
    run_id = state.current_run_id,
    texts = texts,
    inputs = inputs,
  }
  emit("mag", {
    kind = "mag.steer_run",
    id = id,
    run_id = state.current_run_id,
    actor_id = state.lead_program.llm_actor,
    message = { role = "user", content = text, submission_ids = submission_ids },
  })
  return true
end

local function handle_run_steered(body)
  local pending = state.pending_steer
  if pending == nil or body.in_reply_to ~= pending.id then return end
  state.pending_steer = nil
  if body.accepted == true and body.run_id == pending.run_id then
    -- Acceptance is the ownership boundary: the queued text is now part of
    -- model-visible history. Conversation-manager owns its durable projection.
    local turn = state.current_turn
    if turn ~= nil and turn.run_id == pending.run_id then
      for _, input in ipairs(pending.inputs) do
        for _, request_id in ipairs(input.submission_ids or {}) do
          turn.request_ids[#turn.request_ids + 1] = request_id
          request_lifecycle:acquire(request_id, "turn:" .. turn.run_id)
          request_lifecycle:release(request_id, "input:" .. request_id)
        end
      end
      turn.request_ids = RequestLifecycle.copy_ids(turn.request_ids)
    end
    emit("nefor-tui", { kind = "chat.queue.steered" })
    return
  end
  local restored = {}
  for _, input in ipairs(pending.inputs) do restored[#restored + 1] = input end
  for _, input in ipairs(state.pending_user_inputs) do restored[#restored + 1] = input end
  state.pending_user_inputs = restored
  flush_pending_user_inputs()
end

-- Session interrupt-all: gracefully interrupt the current turn and drop
-- everything queued behind it. Unlike the old kill path, the run is NOT killed
-- — it winds down to a final answer, so we keep `current_run_id` and do NOT
-- force idle (the run is still working; it settles through the normal
-- completed path). The deferred relay queue is kept (matching the prior
-- behaviour): a dispatched run's completion still reaches the model on the next
-- submit. A transcript notice makes the interrupt visible.
local function cancel_all()
  local interrupted = interrupt_active_lead_run()
  local dropped_inputs = #state.pending_user_inputs
  state.pending_user_inputs = {}
  state.pending_steer = nil
  nefor.log.info("agentic-loop: cancel_all (graceful interrupt)", {
    interrupted_lead_run = interrupted,
    deferred_queued = #state.deferred_queue,
    dropped_pending_inputs = dropped_inputs,
  })
  return {
    chat = interrupted,
    deferred = #state.deferred_queue,
    pending_inputs = dropped_inputs,
  }
end

-- /new handler: clear turn + queue + canonical history so the next submit
-- starts a fresh conversation. The retained turn-program artifact survives — the
-- program is per-session config, not per-conversation state.
local function new_chat()
  state.current_run_id = nil
  state.current_turn = nil
  state.deferred_queue = {}
  state.pending_user_inputs = {}
  state.pending_steer = nil
  state.conversation:reset()
  state.conversation_id = nil
  state.pending_conversation_create = nil
  state.pending_system_seed = nil
  state.pending_compaction = nil
  state.pending_context_request = nil
  state.ready_announced = false
  state.context_error = nil
  ensure_conversation_id()
end

local function compaction_failure(message, pending)
  pending = pending or {}
  emit("nefor-tui", {
    kind = "chat.compaction.failed",
    provider = pending.provider or state.config.provider,
    model = pending.model or state.config.model,
    trigger = pending.trigger or "manual",
    message = message,
  })
end

local function handle_chat_compaction_request(body)
  if state.pending_compaction ~= nil then
    compaction_failure("context compaction is already in progress")
    return
  end
  if state.current_run_id ~= nil then
    compaction_failure("cannot compact context while a lead turn is running")
    return
  end
  local history = conversation_history()
  if #history == 0 then
    compaction_failure("nothing to compact")
    return
  end

  if not conversation_ready() then
    compaction_failure("conversation context is not ready")
    return
  end
  local request_id = "conversation-compaction-" .. envelope.uuid_lite()
  local pending = { request_id = request_id, trigger = body.trigger or "manual" }
  local run_model_snapshot, snapshot_error = resolve_run_model_snapshot()
  if snapshot_error ~= nil then
    compaction_failure(snapshot_error, pending)
    return
  end
  state.pending_compaction = pending
  local request = {
    kind = "conversation.context.compact.request",
    request_id = request_id,
    conversation_id = state.conversation_id,
    provider = state.config.provider,
    model = state.config.model,
  }
  if run_model_snapshot ~= nil then
    request.provider = run_model_snapshot.provider
    request.model = run_model_snapshot.model
    request.provider_options = run_model_snapshot.provider_options
  end
  emit("conversation-manager", request)
end

-- Mid-chat /model picker. A switch refreshes the manager-owned universal
-- context before another turn may start; the provider edge decides whether
-- its opaque checkpoint is compatible or full history is required.
local function set_model(provider, model)
  if type(provider) == "string" and #provider > 0 then
    -- Reasoning effort is provider-specific vocabulary. Carrying the previous
    -- provider's effort across a cross-provider switch would send a value the
    -- new provider never advertised; the new provider's default applies until
    -- the user selects one again.
    if state.config.provider ~= provider then
      state.config.reasoning_effort = nil
    end
    state.config.provider = provider
  end
  if type(model) == "string" and #model > 0 then
    state.config.model = model
  end
  record_configuration()
end

local function set_reasoning_effort(provider, effort)
  if type(provider) == "string" and #provider > 0 then
    state.config.provider = provider
  end
  if type(effort) ~= "string" or #effort == 0 then return end
  state.config.reasoning_effort = effort
  record_configuration()
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
  local request_id = type(body.submission_id) == "string" and body.submission_id
    or "submission-" .. envelope.uuid_lite()
  local submission_ids = { request_id }
  local sessions = require("libs.sessions")
  request_lifecycle:accept(request_id, sessions.current_id())
  request_lifecycle:acquire(request_id, "input:" .. request_id, sessions.current_id())

  nefor.log.info("agentic-loop: chat.input.submit received", {
    text_len = #text,
    text_preview = string.sub(text, 1, 80),
    busy = state.current_run_id ~= nil,
    deferred_queued = #state.deferred_queue,
    user_queued = #state.pending_user_inputs,
  })

  ensure_conversation_id()
  if state.current_run_id ~= nil or conversation_commit_pending()
      or not conversation_ready() then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = { text = text, submission_ids = submission_ids }
    ensure_lead_program_loaded()
    return
  end

  if state.lead_program.artifact == nil then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = { text = text, submission_ids = submission_ids }
    ensure_lead_program_loaded()
    return
  end

  local deferred, deferred_request_ids, deferred_obligations = drain_deferred_text()
  if type(deferred) == "string" then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = {
      text = text,
      submission_ids = submission_ids,
    }
    submit_orchestrator_run(deferred, deferred_request_ids,
      "internal_async_completion", deferred_obligations)
    return
  end

  submit_orchestrator_run(text, submission_ids, nil, {
    { id = "input:" .. request_id, request_ids = submission_ids },
  })
end

local function handle_chat_reset()
  nefor.log.info("agentic-loop: chat.reset received, clearing turn state", {
    dropped_deferred = #state.deferred_queue,
    dropped_pending_inputs = #state.pending_user_inputs,
    had_run = state.current_run_id ~= nil,
    history_len = #conversation_history(),
  })
  new_chat()
  emit_idle_state("reset")
end

local function handle_chat_model_set(body)
  local model = body.model
  local provider = body.provider
  if type(provider) ~= "string" or provider == ""
      or type(model) ~= "string" or model == "" then return end
  nefor.log.info("agentic-loop: chat.model.set received", {
    provider = provider, model = model, previous = state.config.model,
  })
  state.pending_model_selection = { provider = provider, model = model }
end

local function handle_chat_model_set_ack(body)
  local pending = state.pending_model_selection
  if type(pending) ~= "table" or body.provider ~= pending.provider
      or body.model ~= pending.model then return end
  local provider_changed = state.config.provider ~= pending.provider
  state.config.provider = pending.provider
  state.config.model = pending.model
  if type(body.reasoning_effort) == "string" and body.reasoning_effort ~= "" then
    state.config.reasoning_effort = body.reasoning_effort
  elseif provider_changed then
    state.config.reasoning_effort = nil
  end
  state.pending_model_selection = nil
  record_configuration()
  request_context("model-changed")
end

local function handle_chat_model_set_failed(body)
  local pending = state.pending_model_selection
  if type(pending) ~= "table" or body.provider ~= pending.provider
      or body.model ~= pending.model then return end
  state.pending_model_selection = nil
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

-- The lead run began: retain its scoped wire-id prefix for provider stream
-- and gated-tool correlation. It is deliberately not a surface contract.
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
end

local function typed_semantic_name(result)
  if type(result) ~= "table" or type(result.semantic_type_id) ~= "string"
      or type(result.semantic_type) ~= "table" then
    return nil
  end
  local declared = type(state.lead_program.semantic_types) == "table"
      and state.lead_program.semantic_types[result.semantic_type_id] or nil
  if type(declared) ~= "table" or not deep_equal(result.semantic_type, declared) then
    return nil
  end
  return result.semantic_type.name
end

local function nested_message(value)
  local current = value
  for _ = 1, 8 do
    if type(current) ~= "table" then return nil end
    if type(current.message) == "string" and #current.message > 0 then
      return current.message
    end
    current = current.value
  end
  return nil
end

local function last_output_text(value)
  if type(value) ~= "table" then return nil end
  local last = value.last_output
  if type(last) == "string" and #last > 0 then return last end
  if type(last) ~= "table" then return nil end
  if type(last.text) == "string" and #last.text > 0 then return last.text end
  if type(last.text_answer) == "string" and #last.text_answer > 0 then
    return last.text_answer
  end
  return nil
end

local function error_display(raw, partial)
  raw = type(raw) == "string" and raw
      or "The agent run failed before producing a usable answer."
  local lower = raw:lower()
  if lower:find("overload", 1, true)
      or lower:find("temporarily unavailable", 1, true) then
    return {
      title = "Provider temporarily unavailable",
      message = "The model provider is overloaded right now. Please try again.",
      retryable = true,
      partial = partial,
    }
  end
  if lower:find("auth not connected", 1, true)
      or lower:find("login required", 1, true) then
    return {
      title = "Login required",
      message = "Sign in to the ChatGPT provider before retrying this request.",
      retryable = false,
      partial = partial,
    }
  end
  if lower:find("write-capable agents", 1, true)
      or lower:find("write-review", 1, true) then
    return {
      title = "Approval required",
      message = "This workflow can modify files. Submit its plan for review and approve it before execution.",
      retryable = false,
      partial = partial,
    }
  end
  if lower:find("semantic_type", 1, true)
      or lower:find("constructor_id", 1, true)
      or lower:find("arrival_id", 1, true) then
    raw = "The agent run failed before producing a usable answer."
  end
  return {
    title = "Agent run failed",
    message = raw,
    retryable = false,
    partial = partial,
  }
end

local function direct_agent_error_display(result)
  if typed_semantic_name(result) ~= "nefor.contracts.AgentError"
      or type(result.value) ~= "table" then
    return nil
  end
  return error_display(
    nested_message(result.value.reason),
    last_output_text(result.value)
  )
end

local function result_argument_name(descriptor, index)
  local argument = type(descriptor) == "table" and descriptor.arguments
      and descriptor.arguments[index] or nil
  return type(argument) == "table" and argument.name or nil
end

local function result_constructor_payload_name(descriptor, name)
  for _, constructor in ipairs(type(descriptor) == "table" and descriptor.constructors or {}) do
    if constructor.name == name then
      return type(constructor.payload) == "table" and constructor.payload.name or nil
    end
  end
  return nil
end

-- Decode only the terminal contracts owned by the lead workflow. Result stays
-- nominal in the kernel; this host deliberately projects its Ok/Error branch
-- once, at the presentation boundary.
local function decode_terminal_result(result)
  if type(result) ~= "table" then return { kind = "untyped", value = result } end
  local semantic_name = typed_semantic_name(result)
  if semantic_name == nil then
    if result.semantic_type_id ~= nil or result.semantic_type ~= nil
        or result.constructor_id ~= nil then
      return { kind = "malformed" }
    end
    return { kind = "untyped", value = result, result = result }
  end
  if semantic_name ~= "core.types.Result" then
    local direct_error = direct_agent_error_display(result)
    if direct_error ~= nil then return { kind = "error", display = direct_error } end
    return { kind = "value", value = result.value, result = result }
  end

  local descriptor = result.semantic_type
  local value = result.value
  local error_name = result_argument_name(descriptor, 1)
  local success_name = result_argument_name(descriptor, 2)
  if descriptor.kind ~= "adt" or result.semantic_type_id ~= LEAD_RESULT_TYPE_ID
      or result.constructor_id ~= LEAD_RESULT_CONSTRUCTOR_IDS[value and value.constructor]
      or type(descriptor.arguments) ~= "table" or #descriptor.arguments ~= 2
      or error_name ~= "nefor.contracts.AgentError"
      or success_name ~= "nefor.contracts.TextAnswer"
      or type(value) ~= "table" or type(value.constructor) ~= "string"
      or (value.constructor ~= "Ok" and value.constructor ~= "Error")
      or value.value == nil
      or result_constructor_payload_name(descriptor, value.constructor)
          ~= (value.constructor == "Error" and error_name or success_name) then
    return { kind = "malformed" }
  end
  if value.constructor == "Ok" then
    return { kind = "value", value = value.value, result = result }
  end
  if value.constructor == "Error" and type(value.value) == "table" then
    return {
      kind = "error",
      display = error_display(
        nested_message(value.value.reason),
        last_output_text(value.value)
      ),
    }
  end
  return { kind = "malformed" }
end

local function terminal_value_text(decoded)
  local value = decoded.value
  if type(value) == "string" then return value end
  if type(value) == "table" and type(value.content) == "string" then
    return value.content
  end
  local result = decoded.result
  if decoded.kind == "untyped" and type(result) == "table" then
    if type(result.text) == "string" and #result.text > 0 then return result.text end
    if type(result.text_answer) == "string" and #result.text_answer > 0 then
      return result.text_answer
    end
  end
  if decoded.kind == "value" then
    local ok, encoded = pcall(json.encode, value)
    if ok and type(encoded) == "string" then return encoded end
  end
  return nil
end

-- Terminal close of the lead's turn-program.
--   completed — the answer already painted into the transcript via the
--     prefix-bound stream (exactly how the lead's final answer renders
--     today); when no stream flowed (a non-streaming provider), the text
--     is appended so the turn is never silently empty. Canonical history
--     waits for conversation-manager's correlated terminal turn projection,
--     so the next queued turn can only start from committed context.
--   failed/killed — surfaced locally after the manager has committed the
--     corresponding failed/interrupted terminal projection.
local function finish_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  local run_id = turn.run_id
  local request_ids = turn.request_ids or {}
  local turn_obligation = "turn:" .. run_id
  state.current_run_id = nil
  state.current_turn = nil

  if body.status == "completed" then
    local decoded = decode_terminal_result(body.result)
    if decoded.kind == "error" or decoded.kind == "malformed" then
      local display = decoded.display or {
        title = "Invalid agent result",
        message = "The agent run returned malformed typed terminal data.",
        retryable = false,
      }
      emit("nefor-tui", {
        kind = "chat.error.append",
        title = display.title,
        message = display.message,
        retryable = display.retryable,
      })
      nefor.log.warn("agentic-loop: lead turn returned a business error", {
        run_id = run_id,
        error = display.message,
        history_len = #conversation_history(),
      })
      fire_observers(state.complete_observers, run_id, "error")
      request_lifecycle:set_terminal(request_ids, "error", "", {
        code = decoded.kind == "malformed" and "invalid_terminal_result" or "agent_error",
        message = display.message,
      })
      request_lifecycle:release_all(request_ids, turn_obligation)
      flush_deferred()
      flush_pending_user_inputs()
      emit_idle_if_idle(run_id)
      return
    end
    local answer = terminal_value_text(decoded) or ""
    nefor.log.info("agentic-loop: lead turn completed", {
      run_id = run_id,
      answer_len = #answer, history_len = #conversation_history(),
    })
    fire_observers(state.complete_observers, run_id, "success", answer)
    request_lifecycle:set_terminal(request_ids, "success", answer)
    request_lifecycle:release_all(request_ids, turn_obligation)
    flush_deferred()
    flush_pending_user_inputs()
    emit_idle_if_idle(run_id)
    return
  end

  if body.status == "killed" then
    -- A killed turn (hard kill) still preserves context: the
    -- The manager's interrupted projection already owns the durable context;
    -- the transcript notice from cancel_all remains presentation-only.
    nefor.log.info("agentic-loop: lead turn killed", {
      run_id = run_id, history_len = #conversation_history(),
    })
    fire_observers(state.complete_observers, run_id, "killed")
    request_lifecycle:set_terminal(request_ids, "interrupted", "", {
      code = "interrupted",
      message = "request interrupted",
    })
    request_lifecycle:release_all(request_ids, turn_obligation)
    if #state.pending_user_inputs > 0 then
      flush_pending_user_inputs()
    else
      emit_idle_state("cancelled", run_id)
    end
    return
  end

  -- failed (and anything else terminal we don't recognize).
  local display = error_display(tostring(body.error or body.status or "unknown error"))
  emit("nefor-tui", {
    kind = "chat.error.append",
    title = display.title,
    message = display.message,
    retryable = display.retryable,
  })
  -- Durable failed-turn context was committed by conversation-manager before
  -- this local settlement path became eligible.
  nefor.log.warn("agentic-loop: lead turn failed", {
    run_id = run_id, error = body.error, status = body.status,
    history_len = #conversation_history(),
  })
  fire_observers(state.complete_observers, run_id, tostring(body.status))
  request_lifecycle:set_terminal(request_ids, "error", "", {
    code = tostring(body.status or "failed"),
    message = tostring(body.error or "agent run failed"),
  })
  request_lifecycle:release_all(request_ids, turn_obligation)
  flush_deferred()
  flush_pending_user_inputs()
  emit_idle_if_idle(run_id)
end

local function handle_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  turn.result_body = deep_clone(body)
  -- A lead entry can fail before any provider or conversation-manager activity,
  -- so no terminal projection will arrive to release this exact run. Terminal
  -- failures are already canonical kernel outcomes; settle them immediately.
  if body.status ~= "completed" and body.status ~= "killed" then
    finish_mag_run_result(turn.result_body)
  elseif turn.manager_terminal then
    finish_mag_run_result(turn.result_body)
  end
end

local terminal_turn_changes = {
  turn_completed = true,
  turn_failed = true,
  turn_interrupted = true,
}

local function handle_conversation_projection_delta(body)
  local change = body.change
  if type(change) ~= "table" then return end
  if change.kind == "conversation_created" then
    local provenance = type(change.conversation) == "table"
      and change.conversation.provenance or nil
    local is_pending_root = state.pending_conversation_create ~= nil
        and body.conversation_id == state.conversation_id
    local is_replayed_root = type(provenance) == "table"
        and provenance.surface == "lead"
    if not is_pending_root and not is_replayed_root then return end
  end
  if not state.conversation:apply_delta(body) then return end

  if body.replay ~= true and not replay_window.active()
      and change.kind == "content_chunk_appended" then
    local turn = state.current_turn
    local chunk = change.chunk
    if turn ~= nil and change.turn_id == turn.turn_id and type(chunk) == "table"
        and type(chunk.data) == "string" and chunk.data ~= "" then
      if chunk.kind == "text" then
        fire_observers(state.stream_observers, chunk.data)
      elseif chunk.kind == "reasoning" then
        fire_observers(state.reasoning_observers, chunk.data)
      end
    end
  end

  if change.kind == "conversation_created" then
    state.conversation_id = body.conversation_id
    state.pending_conversation_create = nil
    if body.replay == true or replay_window.active() then return end
    emit("conversation-manager", {
      kind = "conversation.active.set",
      request_id = "conversation-active-" .. envelope.uuid_lite(),
      conversation_id = body.conversation_id,
    })
    if not record_system_prompt(body.conversation_id) then
      flush_pending_user_inputs()
      flush_deferred()
      emit_ready_if_ready()
    end
    return
  end

  local pending_system = state.pending_system_seed
  local completed_message_id = change.message_id
  if completed_message_id == nil and type(change.message) == "table" then
    completed_message_id = change.message.id
  end
  if change.kind == "message_completed" and type(pending_system) == "table"
      and completed_message_id == pending_system.message_id then
    state.pending_system_seed = nil
    flush_pending_user_inputs()
    flush_deferred()
    request_lifecycle:recheck_all()
    emit_ready_if_ready()
    return
  end

  if terminal_turn_changes[change.kind] then
    local turn = state.current_turn
    if turn ~= nil and change.run_id == turn.run_id
        and change.turn_id == turn.turn_id then
      turn.manager_terminal = true
      if turn.result_body ~= nil then finish_mag_run_result(turn.result_body) end
    end
    return
  end

  local pending = state.pending_compaction
  local change_request_id = change.request_id
      or (type(change.compaction) == "table" and change.compaction.request_id)
  if type(pending) == "table" and change_request_id == pending.request_id then
    if change.kind == "context_compaction_completed" then
      state.pending_compaction = nil
      emit("nefor-tui", {
        kind = "chat.compaction.commit",
        request_id = change_request_id,
        provider = state.config.provider,
        model = state.config.model,
        display_summary = "Context compacted.",
      })
      request_context("compaction-completed")
    elseif change.kind == "context_compaction_failed" then
      state.pending_compaction = nil
      local detail = type(change.compaction) == "table" and change.compaction.error
        or change.error
      compaction_failure(error_value.display(detail, "context_compaction_failed",
        "context compaction failed"), pending)
      flush_pending_user_inputs()
      flush_deferred()
    end
  end
end

local function handle_conversation_context_snapshot(body)
  local pending = state.pending_context_request
  if type(pending) ~= "table" or body.request_id ~= pending.id then return end
  state.pending_context_request = nil
  if body.found ~= true or not state.conversation:apply_snapshot(body) then
    state.context_error = "conversation context unavailable"
    fail_open_requests({ code = "context_unavailable", message = state.context_error })
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Conversation context unavailable",
      message = "Conversation manager returned no matching context.",
      retryable = true,
    })
    return
  end
  state.context_error = nil
  flush_pending_user_inputs()
  flush_deferred()
  request_lifecycle:recheck_all()
  emit_ready_if_ready()
  emit_idle_if_idle()
end

local function handle_conversation_rejection(body)
  if body.event_id == state.pending_conversation_create then
    state.pending_conversation_create = nil
    state.conversation_id = nil
    fail_open_requests({ code = "conversation_rejected", message = tostring(body.code) })
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Conversation unavailable",
      message = tostring(body.code or "conversation manager rejected creation"),
      retryable = true,
    })
    return
  end
  local pending_system = state.pending_system_seed
  if type(pending_system) == "table"
      and body.event_id == pending_system.completed_event_id then
    state.pending_system_seed = nil
    fail_open_requests({ code = "conversation_rejected", message = tostring(body.code) })
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Conversation unavailable",
      message = tostring(body.code or "conversation manager rejected the system prompt"),
      retryable = true,
    })
    return
  end
  local pending = state.pending_compaction
  if type(pending) == "table"
      and body.event_id == "compaction:" .. pending.request_id .. ":requested" then
    state.pending_compaction = nil
    compaction_failure(error_value.display(body, "context_compaction_rejected",
      "context compaction rejected"), pending)
    flush_pending_user_inputs()
    flush_deferred()
  end
end

local function handle_conversation_query_rejection(body)
  local pending = state.pending_context_request
  if type(pending) ~= "table" or body.request_id ~= pending.id then return end
  state.pending_context_request = nil
  state.context_error = tostring(body.code or "conversation context query rejected")
  fail_open_requests({ code = "context_unavailable", message = state.context_error })
  emit("nefor-tui", {
    kind = "chat.error.append",
    title = "Conversation context unavailable",
    message = state.context_error,
    retryable = true,
  })
  flush_pending_user_inputs()
  flush_deferred()
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
end

local function teardown_for_session_end()
  kill_active_lead_run()
  release_lead_program()
  state.current_run_id = nil
  state.current_turn   = nil
  state.deferred_queue     = {}
  state.pending_user_inputs = {}
  state.pending_steer       = nil
  state.conversation:reset()
  state.conversation_id = nil
  state.pending_conversation_create = nil
  state.pending_system_seed = nil
  state.pending_compaction = nil
  state.pending_context_request = nil
  state.pending_model_selection = nil
  emit_idle_state("session-ended")
  nefor.log.info("agentic-loop: sessions.session_end → state cleared", {})
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
function M.model_snapshot() return model_snapshot() end
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
  if opts.ambient_context ~= nil then
    if type(opts.ambient_context) ~= "table"
        or type(opts.ambient_context.compose) ~= "function" then
      error("configure: ambient_context must provide compose(base, opts)")
    end
    state.config.ambient_context = opts.ambient_context
  end
  if opts.resolve_model_snapshot ~= nil then
    if type(opts.resolve_model_snapshot) ~= "function" then
      error("configure: resolve_model_snapshot must be a function")
    end
    state.config.resolve_model_snapshot = opts.resolve_model_snapshot
  end
  -- lead_program: where the shipped turn-program lives. `source_dir`
  -- defaults to the config dir (NEFOR_CONFIG_DIR); compositions whose
  -- config dir is not the starter (cli-config) pass it explicitly.
  -- `module_roots`, when present, is the complete ordered MAG module search
  -- path. It is copied so later caller mutation cannot alter live config.
  if type(opts.lead_program) == "table" then
    state.lead_program.project_build = mag_workspace.project_build_options(opts.lead_program.project_build)
    if type(opts.lead_program.source_dir) == "string" and #opts.lead_program.source_dir > 0 then
      state.lead_program.source_dir = opts.lead_program.source_dir
    end
    if type(opts.lead_program.entry) == "string" and #opts.lead_program.entry > 0 then
      state.lead_program.entry = opts.lead_program.entry
    end
    local roots = opts.lead_program.module_roots
    if roots ~= nil then
      if type(roots) ~= "table" or #roots == 0 then
        error("configure: lead_program.module_roots must be a non-empty list of non-empty strings")
      end
      local copy = {}
      for i, root in ipairs(roots) do
        if type(root) ~= "string" or #root == 0 then
          error("configure: lead_program.module_roots[" .. tostring(i)
            .. "] must be a non-empty string")
        end
        copy[i] = root
      end
      local count = 0
      for key, _ in pairs(roots) do
        if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > #roots then
          error("configure: lead_program.module_roots must be a list without extra entries")
        end
        count = count + 1
      end
      if count ~= #roots then
        error("configure: lead_program.module_roots must be a contiguous list")
      end
      state.lead_program.module_roots = copy
    end
  end
end

-- Relay a completed dispatched run to the model as a fresh turn. The
-- completion is formatted into a user-role task (format_deferred) and
-- submitted as a new turn-program once the lead is idle (deferred_queue +
-- flush_deferred). lead-workflow drives this for kernel runs the lead
-- dispatched via its `mag-apply` tool.
-- `completion` shape: { run_id, status = "success"|"failed", output|error }.
function M.relay_run_completion(completion)
  if type(completion) ~= "table" then return false end
  local request_ids = RequestLifecycle.copy_ids(completion.request_ids)
  local obligation_id = "delivery:" .. tostring(completion.run_id or envelope.uuid_lite())
  if #request_ids == 0 then
    state.deferred_queue[#state.deferred_queue + 1] = { text = format_deferred(completion) }
    flush_deferred()
    return true
  end
  request_lifecycle:acquire_all(request_ids, obligation_id)
  local deliverable = {}
  for _, request_id in ipairs(request_ids) do
    if not request_lifecycle:is_forced(request_id) then deliverable[#deliverable + 1] = request_id end
  end
  if #deliverable == 0 then
    request_lifecycle:release_all(request_ids, obligation_id)
    return true
  end
  state.deferred_queue[#state.deferred_queue + 1] = {
    text = format_deferred(completion),
    request_ids = deliverable,
    obligation_id = obligation_id,
  }
  flush_deferred()
  return true
end

-- Explicit seam used by lead-workflow's canonical run registry. Obligations
-- are acquired before an async acknowledgement and released only after result
-- delivery has transferred to a continuation or an owning actor has settled.
function M.acquire_request_obligation(request_ids, obligation_id, session_id)
  request_lifecycle:acquire_all(request_ids, obligation_id, session_id)
end

function M.settle_request_obligation(request_ids, obligation_id)
  request_lifecycle:release_all(request_ids, obligation_id)
end

function M.record_request_outcome(request_ids, status, err)
  request_lifecycle:record_outcome(request_ids, status, err)
end

function M.current_request_ids()
  local turn = state.current_turn
  return RequestLifecycle.copy_ids(turn and turn.request_ids or {})
end

function M.request_is_failing(request_id)
  return request_lifecycle:is_forced(request_id)
end

function M.interrupt_request(request_id)
  if type(request_id) ~= "string" or request_id == "" then return false end
  local changed = request_lifecycle:force(request_id, "interrupted", {
    code = "interrupted",
    message = "request interrupted",
  })
  request_lifecycle:recheck(request_id)
  return changed
end

-- MAG authority loss has no canonical run terminal to consume. Request
-- lifecycle authors only the coarser fact it owns: accepted outcomes are now
-- unknowable, and its correlated obligations are released.
function M.mag_authority_lost(err)
  err = err or {
    code = "mag_authority_lost",
    message = "MAG execution authority was lost before accepted work settled",
  }
  state.current_run_id = nil
  state.current_turn = nil
  state.pending_user_inputs = {}
  state.deferred_queue = {}
  state.pending_steer = nil
  return request_lifecycle:authority_lost(err)
end

function M.prepare()
  -- Fresh sessions deliberately keep the root lazy so the first canonical
  -- chat.input.submit opens persistence before any conversation facts exist.
  -- Resumed roots are rebuilt by replay and refreshed on sessions.resume_done.
  state.prepare_requested = true
  emit_ready_if_ready()
  return true
end

function M.is_ready()
  return loop_ready()
end

function M.fail_request(request_id, err)
  if type(request_id) ~= "string" or request_id == "" then return false end
  if not request_lifecycle:force(request_id, "error", err) then return false end

  local retained = {}
  for _, input in ipairs(state.pending_user_inputs) do
    local ids_for_input, matches = {}, false
    for _, id in ipairs(input.submission_ids or {}) do
      if id == request_id then matches = true else ids_for_input[#ids_for_input + 1] = id end
    end
    if matches then request_lifecycle:release(request_id, "input:" .. request_id) end
    if #ids_for_input > 0 then
      input.submission_ids = ids_for_input
      retained[#retained + 1] = input
    end
  end
  state.pending_user_inputs = retained
  request_lifecycle:release(request_id, "input:" .. request_id)

  local deferred = {}
  for _, entry in ipairs(state.deferred_queue) do
    local retained_ids, matched = {}, false
    for _, id in ipairs(entry.request_ids or {}) do
      if id == request_id then matched = true else retained_ids[#retained_ids + 1] = id end
    end
    if matched then
      request_lifecycle:release(request_id, entry.obligation_id)
    end
    if #retained_ids > 0 or #(entry.request_ids or {}) == 0 then
      entry.request_ids = retained_ids
      deferred[#deferred + 1] = entry
    end
  end
  state.deferred_queue = deferred

  local turn = state.current_turn
  if turn ~= nil then
    for _, id in ipairs(turn.request_ids or {}) do
      if id == request_id then kill_active_lead_run(); break end
    end
  end
  local ok, workflow = pcall(require, "libs.lead-workflow")
  if ok and type(workflow.cancel_request) == "function" then
    workflow.cancel_request(request_id, err)
  end
  request_lifecycle:recheck(request_id)
  return true
end

fail_open_requests = function(err)
  for request_id, request in pairs(request_lifecycle.requests) do
    if not request.completed then M.fail_request(request_id, err) end
  end
end

-- Whether a gate correlation id belongs to the lead's ACTIVE turn (ids are
-- `<scope>/cap-N`; the lead's own tool firings carry its turn scope, a
-- dispatched sub-run's carry that run's scope). Public for consumers that
-- need to classify a capability correlation against the active lead turn.
function M.lead_scoped_id(id) return lead_scoped_id(id) end

function M.fire_tool_start_observers(id, name, input) fire_tool_start_observers(id, name, input) end
function M.fire_tool_end_observers(id, output, err) fire_tool_end_observers(id, output, err) end
function M.set_reasoning_effort(provider, effort) set_reasoning_effort(provider, effort) end

function M._teardown_for_session_end() return teardown_for_session_end() end

function M.config() return state.config end

-- Disposable read-only cache of conversation-manager's universal messages.
function M.history() return conversation_history() end

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

  if kind == "conversation.projection.delta" then
    handle_conversation_projection_delta(body)
    return
  end
  if kind == "conversation.context.snapshot" then
    handle_conversation_context_snapshot(body)
    return
  end
  if kind == "conversation.fact.rejected" then
    handle_conversation_rejection(body)
    return
  end
  if kind == "conversation.query.rejected" then
    handle_conversation_query_rejection(body)
    return
  end

  if replay_window.active() then
    -- conversation-manager rebuilds and publishes the universal projection;
    -- input handlers must not
    -- re-fire (a replayed chat.input.submit would spawn a fresh turn
    -- the user already saw the answer for).
    if kind == "chat.input.submit"
        or kind == "chat.steer"
        or kind == "chat.reset"
        or kind == "chat.interrupt"
        or kind == "chat.interrupt_all"
        or kind == "chat.model.set"
        or kind == "chat.reasoning.set"
        or kind == "chat.compaction.request" then
      return
    end
    -- Everything else during replay: kernel/turn lifecycle events are
    -- keyed to the (empty) live turn state and fall through harmlessly.
    return
  end

  if kind == "chat.input.submit" then handle_chat_input_submit(body); return end
  if kind == "chat.reset"        then handle_chat_reset(); return end
  if kind == "chat.steer"        then steer_pending_inputs(); return end
  if kind == "chat.interrupt"    then cancel(body.drop_queued == true); return end
  if kind == "chat.interrupt_all" then cancel_all(); return end
  if kind == "chat.model.set" then handle_chat_model_set(body); return end
  if kind == "chat.model.set_ack" then handle_chat_model_set_ack(body); return end
  if kind == "chat.model.set_failed" then handle_chat_model_set_failed(body); return end
  if kind == "chat.reasoning.set" then handle_chat_reasoning_set(body); return end
  if kind == "chat.compaction.request" then handle_chat_compaction_request(body); return end

  -- Turn-program load handshake.
  if kind == "mag.loaded" then handle_lead_program_loaded(body); return end
  if kind == "mag.error"  then handle_lead_program_error(body); return end

  -- Turn lifecycle.
  if kind == "mag.run_started" then handle_mag_run_started(body); return end
  if kind == "mag.run_steered" then handle_run_steered(body); return end
  if kind == "mag.run_result"  then handle_mag_run_result(body); return end

  -- Lead-scoped tool + stream observation.
  if type(kind) == "string" then
    if kind:match("%.tool%.invoke$") then handle_gate_invoke(body); return end
    if kind == "tool.result" then handle_gate_result(body); return end
  end
end

local function restore_configuration_from_projection()
  local provenance = state.conversation:provenance()
  local changed = false
  for _, key in ipairs({ "provider", "model", "reasoning_effort" }) do
    local value = provenance[key]
    if type(value) == "string" and value ~= "" and state.config[key] ~= value then
      state.config[key] = value
      changed = true
    end
  end
  if not changed then return end
  nefor.log.info("agentic-loop: /resume restored canonical conversation configuration", {
    provider = state.config.provider,
    model = state.config.model,
    reasoning_effort = state.config.reasoning_effort,
  })
  if type(state.config.model) == "string" and state.config.model ~= "" then
    emit(nil, {
      kind = "chat.model.set_ack",
      provider = state.config.provider,
      model = state.config.model,
    })
  end
  if type(state.config.reasoning_effort) == "string"
      and state.config.reasoning_effort ~= "" then
    emit(nil, {
      kind = "chat.reasoning.set_ack",
      provider = state.config.provider,
      effort = state.config.reasoning_effort,
    })
  end
end

-- Drive `teardown_for_session_end` from the bus marker. Replay-mode
-- gating is owned by `core.replay_window`, which subscribes to
-- `sessions.replay.start` / `sessions.replay.end` independently.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(_entry)
    state.prepare_requested = false
    state.ready_announced = false
    teardown_for_session_end()
    -- Modes are session-scoped authority. A session switch must reset the
    -- live gate rather than letting the previous session's process state leak
    -- across the boundary; explicit startup mode is applied separately.
    set_mode("safe")
  end)
  -- Replay is chunked; settle the restored conversation only after the whole
  -- resume, never at each chunk boundary.
  nefor.bus.on_event("sessions.resume_done", function(_entry)
    state.prepare_requested = true
    state.ready_announced = false
    if conversation_ready() then
      restore_configuration_from_projection()
      request_context("resume")
    else
      emit_ready_if_ready()
    end
  end)
end

M.name        = "agentic-loop"
M.receive_msg = receive_msg
M.send_msg    = function(_) end  -- no internal-output translation
M._internals  = {
  state = state,
  request_lifecycle = request_lifecycle,
  reset = function()
    state.config = {
      provider = "ollama",
      model = nil,
      reasoning_effort = nil,
      system = nil,
      ambient_context = nil,
      resolve_model_snapshot = nil,
    }
    state.pending_model_selection = nil
    state.lead_program = {
      source_dir = nil,
      entry = "agentic-loop/lead-turn.mag",
      module_roots = nil,
      artifact = nil,
      hash = nil,
      source_actor = nil,
      entry_actor = nil,
      llm_actor = nil,
      load_id = nil,
    }
    state.conversation:reset()
    state.conversation_id = nil
    state.pending_conversation_create = nil
    state.pending_system_seed = nil
    state.pending_compaction = nil
    state.pending_context_request = nil
    state.prepare_requested = false
    state.ready_announced = false
    state.context_error = nil
    state.current_run_id = nil
    state.current_turn = nil
    state.deferred_queue = {}
    state.pending_user_inputs = {}
    state.pending_steer = nil
    state.stream_observers = {}
    state.reasoning_observers = {}
    state.tool_start_observers = {}
    state.tool_end_observers = {}
    state.complete_observers = {}
    request_lifecycle:reset()
    state.mag_context = {
      workspace = nil,
      workspace_session = nil,
    }
    envelope._reset()
  end,
}

return M
