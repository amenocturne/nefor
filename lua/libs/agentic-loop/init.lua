-- lua/libs/agentic-loop/init.lua — the lead's turn spawner.
--
-- The lead's turn is a short-lived MAG program over a persistent chat:
-- turn-as-function, `(conversation, message) -> response`. Per user message this
-- actor clones the shipped turn-program (agentic-loop/lead-turn.mag,
-- compiled once via `mag.load` and cached), sets the initial `mag.Task`
-- payload to the message, overlays the live provider/model/reasoning effort
-- plus the canonical conversation identity onto the lead
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

local state = {
  -- Orchestrator config — mutated by configure() / chat.model.set.
  config = {
    provider         = "ollama",
    model            = nil,
    reasoning_effort = nil,
    system           = nil,
  },

  -- The shipped turn-program. `source_dir`/`entry` are composition-owned
  -- (configure { lead_program = … }); the artifact is loaded once per
  -- session through the mag plugin and cached here, then cloned per turn.
  lead_program = {
    source_dir = nil,   ---@type string|nil  resolved lazily (NEFOR_CONFIG_DIR)
    entry      = "agentic-loop/lead-turn.mag",
    module_roots = nil, ---@type string[]|nil explicit ordered search roots
    artifact    = nil,   ---@type table|nil   cached compiled artifact
    hash       = nil,   ---@type string|nil
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

  -- Ambient MAG context recorded once in the conversation's system message so the
  -- lead can start writing MAG without a discovery round-trip. `static`
  -- (inventory + patterns + types + template signatures + prompt roster)
  -- is read once from the config lib dir and cached process-wide; the
  -- `workspace` dir is per-session. `static_builds` counts real (re)reads
  -- so a test can prove the cache holds across turns.
  mag_context = {
    static            = nil,  ---@type string|nil
    static_builds     = 0,    ---@type number
    workspace         = nil,  ---@type string|nil
    workspace_session = nil,  ---@type string|nil
  },
}

local emit           = envelope.emit

local format_deferred = results_lib.format_deferred

local function conversation_history()
  return state.conversation:history()
end

local function conversation_commit_pending()
  return state.pending_compaction ~= nil
      or state.pending_context_request ~= nil
end

local function append_conversation_fact(fact)
  emit("conversation-manager", {
    kind = "conversation.fact.append",
    fact = fact,
  })
end

local function configuration_provenance()
  return {
    surface = "lead",
    provider = state.config.provider,
    model = state.config.model,
    reasoning_effort = state.config.reasoning_effort,
  }
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
--
-- The lead used to pay a `mag-env` round-trip (plus reading patterns.md)
-- before writing any MAG. That context is now ambient: appended to the
-- conversation's canonical system message. Contents: the session workspace
-- dir, the seeded
-- lib/ inventory, patterns.md inlined (the canonical authoring contract),
-- and the prompt roster.
--
-- Seam: the MAG workspace is lead-workflow's domain, but its path
-- resolution and seeding live in the shared `libs.mag-workspace` module
-- (libs.mag-workspace) — required here as a composition-layer helper, not a reach
-- into lead-workflow's internals.

-- The config dir that holds the turn-program and the mag/lib library. Same
-- resolution as lead_program_source_dir (defined below for the load path);
-- inlined here to stay independent of definition order.
local function mag_config_dir()
  local sd = state.lead_program.source_dir
  if type(sd) == "string" and #sd > 0 then return sd end
  return rawget(_G, "NEFOR_CONFIG_DIR") or os.getenv("NEFOR_CONFIG_DIR") or "."
end

local function read_config_file(path)
  if not (nefor.fs and type(nefor.fs.read_file) == "function") then return nil end
  local ok, res = pcall(nefor.fs.read_file, path)
  if ok and type(res) == "table" and res.ok and type(res.content) == "string" then
    return res.content
  end
  return nil
end

-- Sorted relative inventory of the config lib dir (top-level files plus one
-- level of subdirectories, e.g. prompts/*).
local function lib_inventory(lib_dir)
  if not (nefor.fs and type(nefor.fs.list_dir) == "function") then return {} end
  local rels = {}
  local top = nefor.fs.list_dir(lib_dir)
  for _, e in ipairs(top or {}) do
    if e.is_dir then
      local sub = nefor.fs.list_dir(lib_dir .. "/" .. e.name)
      for _, s in ipairs(sub or {}) do
        if not s.is_dir then rels[#rels + 1] = e.name .. "/" .. s.name end
      end
    else
      rels[#rels + 1] = e.name
    end
  end
  table.sort(rels)
  return rels
end

-- Names (no extension) of the prompt roster under lib/prompts/.
local function prompt_names(lib_dir)
  if not (nefor.fs and type(nefor.fs.list_dir) == "function") then return {} end
  local names = {}
  local entries = nefor.fs.list_dir(lib_dir .. "/prompts")
  for _, e in ipairs(entries or {}) do
    if not e.is_dir then names[#names + 1] = (e.name:gsub("%.md$", "")) end
  end
  table.sort(names)
  return names
end

-- Build the static (config-derived) section. Returns (text, complete) where
-- `complete` is true when patterns.md was readable — an incomplete build is
-- not cached, so a later turn (once NEFOR_CONFIG_DIR resolves) rebuilds it.
local function build_mag_static_section(config_dir)
  local lib_dir  = config_dir .. "/mag/lib"
  local patterns = read_config_file(lib_dir .. "/patterns.md")

  local parts = {}
  parts[#parts + 1] = "The session MAG workspace is seeded and ready. Paths you pass to " ..
    "`mag` are relative to it. The current canonical authoring contract from " ..
    "patterns.md is inlined below; the composition provides ready agent primitives."
  parts[#parts + 1] = ""
  parts[#parts + 1] = "lib/ inventory:"
  for _, rel in ipairs(lib_inventory(lib_dir)) do parts[#parts + 1] = "  " .. rel end
  parts[#parts + 1] = ""
  parts[#parts + 1] = "### lib/patterns.md"
  parts[#parts + 1] = patterns or "(unavailable)"
  local names = prompt_names(lib_dir)
  if #names > 0 then
    parts[#parts + 1] = ""
    parts[#parts + 1] = "### lib/prompts/ (reusable task steering fragments)"
    parts[#parts + 1] = "  " .. table.concat(names, ", ")
  end

  return table.concat(parts, "\n"), (patterns ~= nil)
end

-- Cached static section. Read once; cache only a complete build.
local function mag_static_section(config_dir)
  local mc = state.mag_context
  if type(mc.static) == "string" then return mc.static end
  local text, complete = build_mag_static_section(config_dir)
  mc.static_builds = mc.static_builds + 1
  if complete then mc.static = text end
  return text
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
local function mag_workspace_block()
  local sessions = require("libs.sessions")
  local session_id = sessions.current_id()
  if type(session_id) ~= "string" or session_id == "" then return nil end
  local config_dir = mag_config_dir()
  local ws = mag_workspace_dir(session_id, config_dir)
  local lines = {
    "## MAG workspace",
    "",
    "workspace dir: " .. tostring(ws),
    "",
    mag_static_section(config_dir),
  }
  return table.concat(lines, "\n")
end

-- Append the ambient MAG context to the conversation's system prompt. The
-- block is additive: an empty base system prompt still carries the context.
local function system_with_mag_context(base)
  local block = mag_workspace_block()
  if type(block) ~= "string" then return base end
  if type(base) == "string" and #base > 0 then
    return base .. "\n\n" .. block
  end
  return block
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

-- Deep-copy JSON-shaped data (the cached modification is cloned per turn so
-- per-turn mutation never leaks into the cache). Decoded JSON
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

-- Derive the turn-program's seams from its compiled modification:
--   * source actor — the initial Unit message's target and task-value owner;
--   * entry actor — the source value's destination;
--   * lead llm — the llm-factory actor the entry adapter routes
--     `generic-provider.ProviderOut` into (the overlay + binding target).
-- Derivation over hardcoding keeps the program hackable: rename the agent
-- in lead-turn.mag and the spawner follows.
local function derive_program_seams(modification)
  local msg = (modification.messages or {})[1]
  local source_actor = type(msg) == "table" and msg.to or nil
  if type(source_actor) ~= "string" then
    return nil, "turn-program has no initial message (no source actor)"
  end
  local entry_actor
  local llm_actor
  for _, actor in ipairs(modification.actors or {}) do
    if actor.id == source_actor then
      local destinations = type(actor.routes) == "table"
        and actor.routes["nefor.graph.Value"] or nil
      local destination = type(destinations) == "table" and destinations[1] or nil
      entry_actor = type(destination) == "table" and destination.actor or nil
      if type(actor.params) ~= "table" or type(actor.params.value) ~= "table" then
        return nil, "turn-program source actor has no typed task value"
      end
    end
  end
  if type(entry_actor) ~= "string" then
    return nil, "turn-program source routes no task value (no entry actor)"
  end
  for _, actor in ipairs(modification.actors or {}) do
    if actor.id == entry_actor then
      local dests = type(actor.routes) == "table"
        and actor.routes["generic-provider.ProviderOut"] or nil
      local destination = type(dests) == "table" and dests[1] or nil
      llm_actor = type(destination) == "table" and destination.actor or nil
    end
  end
  if type(llm_actor) ~= "string" then
    return nil, "turn-program entry actor '" .. tostring(entry_actor)
      .. "' routes no ProviderInput (no lead llm actor)"
  end
  return { source_actor = source_actor, entry_actor = entry_actor, llm_actor = llm_actor }, nil
end

-- Kick the turn-program load handshake (idempotent while in flight). The
-- mag.loaded reply caches the modification and flushes queued submits.
local function ensure_lead_program_loaded()
  local p = state.lead_program
  if p.artifact ~= nil or p.load_id ~= nil then return end
  p.load_id = "lead-turn-load-" .. envelope.uuid_lite()
  local source_dir = lead_program_source_dir()
  local module_roots = lead_program_module_roots(source_dir)
  emit("mag", {
    kind       = "mag.load",
    id         = p.load_id,
    source_dir = source_dir,
    module_roots = module_roots,
    entry      = p.entry,
  })
  nefor.log.info("agentic-loop: loading lead turn-program", {
    source_dir = source_dir, entry = p.entry,
  })
end

local flush_pending_user_inputs

local function handle_lead_program_loaded(body)
  local p = state.lead_program
  if body.in_reply_to ~= p.load_id then return end
  p.load_id = nil
  local artifact = body.artifact
  local modification = type(artifact) == "table" and artifact.data or nil
  if type(modification) ~= "table" then
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Lead program unavailable",
      message = "The lead turn program loaded without executable data.",
      retryable = true,
    })
    return
  end
  local seams, err = derive_program_seams(modification)
  if not seams then
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Lead program invalid",
      message = tostring(err),
      retryable = false,
    })
    return
  end
  p.artifact = artifact
  p.hash = body.hash
  p.source_actor = seams.source_actor
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
    kind = "chat.error.append",
    title = "Lead program failed to compile",
    message = tostring(body.message),
    retryable = true,
  })
  -- Queued submits stay queued; the next submit retries the load.
  emit_idle_state("lead-program-load-failed")
end

-- Spawn one turn-program for `user_text`. Clones the cached modification,
-- points the initial mag.Task at the message, overlays live config +
-- canonical conversation identity onto the lead llm actor, and submits
-- `mag.execute`. The provider actor reads history from conversation-manager;
-- duplicating it in this persisted command would make session growth quadratic.
local function submit_orchestrator_run(user_text, submission_ids, input_cause)
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

  local artifact = deep_clone(p.artifact)
  local mod = artifact.data
  for _, actor in ipairs(mod.actors or {}) do
    if actor.id == p.source_actor then
      actor.params.value.prompt = user_text
    end
  end

  local overlay_params = {
    conversation_id = conversation_id,
    submission_ids = submission_ids,
    input_cause = input_cause,
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
  local run_id = ids.mint_chat_run_id()
  state.current_run_id = run_id
  state.current_turn = {
    run_id    = run_id,
    turn_id   = run_id,
    user_text = user_text or "",
    scope     = nil,
    manager_terminal = false,
    result_body = nil,
  }
  emit_runtime_state("agentic_loop.run_start", { run_id = run_id })

  local sessions = require("libs.sessions")
  envelope.emit_as("agentic-loop", "mag", {
    kind           = "mag.execute",
    id             = run_id,
    run_id         = run_id,
    turn_id        = run_id,
    run_name       = "lead",
    conversation_id = conversation_id,
    session_id     = sessions.current_id(),
    principal      = "lead",
    artifact       = artifact,
    params_overlay = { [p.llm_actor] = overlay_params },
  })
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
  local parts = {}
  for _, entry in ipairs(state.deferred_queue) do
    if type(entry.text) == "string" and #entry.text > 0 then
      parts[#parts + 1] = entry.text
    end
  end
  state.deferred_queue = {}
  if #parts == 0 then return nil end
  return table.concat(parts, "\n\n---\n\n")
end

-- Deferred relay queue. Carries any text that needs to land as the next
-- turn's user-role task: dispatched kernel-run completion bodies relayed
-- by lead-workflow and mag-eval (relay_run_completion).
local function flush_deferred()
  if state.current_run_id ~= nil then return end
  if conversation_commit_pending() or not conversation_ready() then return end
  local merged = drain_deferred_text()
  if type(merged) ~= "string" then return end
  nefor.log.info("agentic-loop: flushing deferred run completions", {
    text_preview = string.sub(merged, 1, 80),
  })
  submit_orchestrator_run(merged, nil, "internal_async_completion")
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
  submit_orchestrator_run(combined, submission_ids)
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
-- starts a fresh conversation. The turn-program cache survives — the
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
  state.pending_compaction = pending
  emit("conversation-manager", {
    kind = "conversation.context.compact.request",
    request_id = request_id,
    conversation_id = state.conversation_id,
    provider = state.config.provider,
    model = state.config.model,
  })
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
  local submission_ids = type(body.submission_id) == "string" and { body.submission_id } or {}
  if type(text) ~= "string" or #text == 0 then return end

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

  local deferred = drain_deferred_text()
  if type(deferred) == "string" then
    state.pending_user_inputs[#state.pending_user_inputs + 1] = {
      text = text,
      submission_ids = submission_ids,
    }
    submit_orchestrator_run(deferred, nil, "internal_async_completion")
    return
  end

  submit_orchestrator_run(text, submission_ids)
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
  if type(model) == "string" and #model > 0 then
    nefor.log.info("agentic-loop: chat.model.set received", {
      provider = provider, model = model, previous = state.config.model,
    })
    set_model(provider, model)
    request_context("model-changed")
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
  if type(result) ~= "table" or type(result.semantic_type_id) ~= "string" then
    return nil
  end
  return type(result.semantic_type) == "table" and result.semantic_type.name or nil
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
  if type(last.final_answer) == "string" and #last.final_answer > 0 then
    return last.final_answer
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

local function agent_error_display(result)
  if typed_semantic_name(result) ~= "nefor.contracts.AgentError"
      or type(result.value) ~= "table" then
    return nil
  end
  return error_display(
    nested_message(result.value.reason),
    last_output_text(result.value)
  )
end

-- The relay text for a successful run result's inline `result` (the sink's
-- final answer riding mag.run_result). Typed AgentError is handled separately
-- so its runtime envelope can never fall through to the JSON encode fallback.
local function mag_result_text(result)
  if type(result) ~= "table" then return nil end
  local semantic_name = typed_semantic_name(result)
  local typed = semantic_name ~= nil
  if typed and semantic_name ~= "nefor.contracts.AgentError" then
    if type(result.value) == "string" then return result.value end
    if type(result.value) == "table" and type(result.value.content) == "string" then
      return result.value.content
    end
  elseif typed then
    return nil
  end
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
--     waits for conversation-manager's correlated terminal turn projection,
--     so the next queued turn can only start from committed context.
--   failed/killed — surfaced locally after the manager has committed the
--     corresponding failed/interrupted terminal projection.
local function finish_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  local run_id = turn.run_id
  state.current_run_id = nil
  state.current_turn = nil

  if body.status == "completed" then
    local agent_error = agent_error_display(body.result)
    if agent_error ~= nil then
      emit("nefor-tui", {
        kind = "chat.error.append",
        title = agent_error.title,
        message = agent_error.message,
        retryable = agent_error.retryable,
      })
      nefor.log.warn("agentic-loop: lead turn returned AgentError", {
        run_id = run_id,
        error = agent_error.message,
        history_len = #conversation_history(),
      })
      fire_observers(state.complete_observers, run_id, "error")
      flush_deferred()
      flush_pending_user_inputs()
      emit_idle_if_idle(run_id)
      return
    end
    local answer = mag_result_text(body.result) or ""
    nefor.log.info("agentic-loop: lead turn completed", {
      run_id = run_id,
      answer_len = #answer, history_len = #conversation_history(),
    })
    fire_observers(state.complete_observers, run_id, "success", answer)
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
  flush_deferred()
  flush_pending_user_inputs()
  emit_idle_if_idle(run_id)
end

local function handle_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  turn.result_body = deep_clone(body)
  if turn.manager_terminal then finish_mag_run_result(turn.result_body) end
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
    emit("nefor-tui", {
      kind = "chat.error.append",
      title = "Conversation context unavailable",
      message = "Conversation manager returned no matching context.",
      retryable = true,
    })
    return
  end
  flush_pending_user_inputs()
  flush_deferred()
  emit_idle_if_idle()
end

local function handle_conversation_rejection(body)
  if body.event_id == state.pending_conversation_create then
    state.pending_conversation_create = nil
    state.conversation_id = nil
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
  emit("nefor-tui", {
    kind = "chat.error.append",
    title = "Conversation context unavailable",
    message = tostring(body.code or "conversation context query rejected"),
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
  -- `module_roots`, when present, is the complete ordered MAG module search
  -- path. It is copied so later caller mutation cannot alter live config.
  if type(opts.lead_program) == "table" then
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
-- dispatched via its `mag` tool.
-- `completion` shape: { run_id, status = "success"|"failed", output|error }.
function M.relay_run_completion(completion)
  if type(completion) ~= "table" then return end
  state.deferred_queue[#state.deferred_queue + 1] = { text = format_deferred(completion) }
  flush_deferred()
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
    teardown_for_session_end()
    -- Modes are session-scoped authority. A session switch must reset the
    -- live gate rather than letting the previous session's process state leak
    -- across the boundary; explicit startup mode is applied separately.
    set_mode("safe")
  end)
  -- Replay is chunked; settle the restored conversation only after the whole
  -- resume, never at each chunk boundary.
  nefor.bus.on_event("sessions.resume_done", function(_entry)
    if conversation_ready() then
      restore_configuration_from_projection()
      request_context("resume")
    end
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
    state.mag_context = {
      static = nil,
      static_builds = 0,
      workspace = nil,
      workspace_session = nil,
    }
    envelope._reset()
  end,
}

return M
