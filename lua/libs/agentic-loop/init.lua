-- lua/libs/agentic-loop/init.lua — the lead's turn spawner.
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
--   * canonical history — per completed turn the llm's FULL transcript delta
--     (user task, assistant tool-call turns, tool results, final answer —
--     ridden back on `mag.run_result result.transcript_delta`, see
--     factories/llm.lua "Transcript delta") is appended and seeded into the
--     next turn's llm via `params.history`, so the next turn replays what the
--     model SAW, not just what it said. A turn that ends without a delta
--     (killed/failed, or a non-llm program) falls back to the bare
--     `{ user message, answer }` pair. `agentic_loop.turn_recorded` markers
--     on the bus make the history rebuildable on /resume.
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
local results_lib     = require("libs.agentic-loop.results")
local history_replay  = require("core.history_replay")
local session_config  = require("libs.agentic-loop.session_config")

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

  -- Canonical conversation history: provider-dialect messages, the full
  -- transcript delta per completed turn (bare user+assistant pair when a
  -- turn ends without one). Seeded into each turn's llm via params.history;
  -- rebuilt on /resume from turn_recorded markers.
  history = {},                 ---@type table

  current_run_id = nil,         ---@type string|nil
  -- The in-flight turn: { run_id, user_text, scope, chat_prefix, streamed }.
  current_turn   = nil,         ---@type table|nil
  deferred_queue       = {},    ---@type table  queued relay texts { text }
  pending_user_inputs  = {},    ---@type table  queued submits while busy
  pending_inputs_projected = 0, ---@type integer cold-queue prefix already projected
  pending_steer        = nil,   ---@type table|nil queued inputs awaiting MAG steer ack

  -- Observer registries. Public on_* setters append; producers fire via
  -- pcall so a bad observer doesn't break the chain.
  stream_observers       = {},  ---@type table
  reasoning_observers    = {},  ---@type table
  tool_start_observers   = {},  ---@type table
  tool_end_observers     = {},  ---@type table
  complete_observers     = {},  ---@type table

  -- Ambient MAG context injected into each turn's system overlay so the
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

-- ── ambient MAG context ───────────────────────────────────────────────
--
-- The lead used to pay a `mag-env` round-trip (plus reading patterns.md)
-- before writing any MAG. That context is now ambient: appended to every
-- turn's `system` overlay. Contents: the session workspace dir, the seeded
-- lib/ inventory, patterns.md inlined (the canonical authoring contract),
-- and the prompt roster.
--
-- Seam: the MAG workspace is lead-workflow's domain, but its path
-- resolution and seeding live in the shared `mag` workspace module
-- (starter/mag) — required here as a composition-layer helper, not a reach
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
  local mag = require("mag")
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
  local sessions = require("sessions")
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

-- Append the ambient MAG context to a turn's system prompt. The block is
-- additive: an empty base system prompt still carries the context.
local function system_with_mag_context(base)
  local block = mag_workspace_block()
  if type(block) ~= "string" then return base end
  if type(base) == "string" and #base > 0 then
    return base .. "\n\n" .. block
  end
  return block
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
      kind = "chat.message.append", role = "system",
      text = "[lead turn-program load carried no artifact data]",
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
  p.artifact = artifact
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
  if p.artifact == nil then
    -- Program not compiled yet: queue the text and (re)kick the load; the
    -- mag.loaded reply flushes the queue.
    state.pending_user_inputs[#state.pending_user_inputs + 1] = user_text
    ensure_lead_program_loaded()
    return nil
  end

  local artifact = deep_clone(p.artifact)
  local mod = artifact.data
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
  -- Ambient MAG context: append the workspace/lib block to the system
  -- overlay so the lead can write MAG immediately (no mag-env round-trip).
  -- Additive — an empty base system prompt still carries the block.
  local base_system = (type(state.config.system) == "string" and #state.config.system > 0)
    and state.config.system or nil
  local system = system_with_mag_context(base_system)
  if type(system) == "string" and #system > 0 then
    overlay_params.system = system
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
  envelope.emit_as("agentic-loop", "mag", {
    kind           = "mag.execute",
    id             = run_id,
    run_id         = run_id,
    run_name       = "lead",
    session_id     = sessions.current_id(),
    principal      = "lead",
    artifact       = artifact,
    params_overlay = { [p.llm_actor] = overlay_params },
  })
  nefor.log.info("agentic-loop: lead turn submitted to mag kernel", {
    run_id = run_id,
    text_preview = string.sub(user_text or "", 1, 80),
    history_len = #state.history,
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
  local merged = drain_deferred_text()
  if type(merged) ~= "string" then return end
  nefor.log.info("agentic-loop: flushing deferred run completions", {
    text_preview = string.sub(merged, 1, 80),
  })
  submit_orchestrator_run(merged)
end

flush_pending_user_inputs = function()
  if state.current_run_id ~= nil then return end
  if #state.pending_user_inputs == 0 then return end
  local inputs = state.pending_user_inputs
  local combined = table.concat(inputs, "\n")
  nefor.log.info("agentic-loop: flushing queued user inputs", {
    count = #inputs,
    text_preview = string.sub(combined, 1, 80),
  })
  local projected_count = state.pending_inputs_projected
  state.pending_user_inputs = {}
  state.pending_inputs_projected = 0
  -- The first cold submit is projected immediately. Later cold submits are
  -- optimistic queued text, so promotion replaces only that unprojected suffix.
  emit("nefor-tui", { kind = "chat.queue.steered" })
  if projected_count < #inputs then
    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "user",
      text = table.concat(inputs, "\n", projected_count + 1),
    })
  end
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

-- Graceful interrupt of the active lead run (NOT a kill). The kernel settles
-- whatever capability the run is blocked on as a
-- failed "interrupted by user" result and cancels the real work (a bash
-- subprocess dies, a provider round aborts); the lead re-fires with that
-- failure in context and winds the turn down with a real final answer. The run
-- SURVIVES — `current_run_id` stays set — so the turn closes through the normal
-- `mag.run_result status:"completed"` path: history records itself and
-- `agentic_loop.turn_recorded` rides the bus. That is what structurally kills
-- the amnesia: there is no killed-without-record turn on this path.
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
    state.pending_inputs_projected = 0
    state.pending_steer = nil
  end
  kill_active_lead_run()
end

local function steer_pending_inputs()
  if state.current_run_id == nil or state.pending_steer ~= nil then return false end
  if #state.pending_user_inputs == 0 then return false end
  local texts = state.pending_user_inputs
  state.pending_user_inputs = {}
  state.pending_inputs_projected = 0
  local text = table.concat(texts, "\n")
  local id = "lead-steer-" .. envelope.uuid_lite()
  state.pending_steer = {
    id = id,
    run_id = state.current_run_id,
    texts = texts,
  }
  emit("mag", {
    kind = "mag.steer_run",
    id = id,
    run_id = state.current_run_id,
    actor_id = state.lead_program.llm_actor,
    message = { role = "user", content = text },
  })
  return true
end

local function handle_run_steered(body)
  local pending = state.pending_steer
  if pending == nil or body.in_reply_to ~= pending.id then return end
  state.pending_steer = nil
  if body.accepted == true and body.run_id == pending.run_id then
    -- Acceptance is the ownership boundary: the queued text is now part of
    -- model-visible history, so emit its durable transcript projection once.
    emit("nefor-tui", { kind = "chat.queue.steered" })
    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "user",
      text = table.concat(pending.texts, "\n"),
    })
    return
  end
  local restored = {}
  for _, text in ipairs(pending.texts) do restored[#restored + 1] = text end
  for _, text in ipairs(state.pending_user_inputs) do restored[#restored + 1] = text end
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
  state.pending_inputs_projected = 0
  state.pending_steer = nil
  if interrupted then
    emit("nefor-tui", {
      kind = "chat.message.append",
      role = "system",
      text = "[interrupted by user — cancelling in-flight work]",
    })
  end
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
  state.pending_inputs_projected = 0
  state.pending_steer = nil
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
    return
  end

  if state.lead_program.artifact == nil then
    local first_cold_input = #state.pending_user_inputs == 0
    state.pending_user_inputs[#state.pending_user_inputs + 1] = text
    if first_cold_input then
      state.pending_inputs_projected = 1
      emit("nefor-tui", {
        kind = "chat.message.append",
        role = "user",
        text = text,
      })
    end
    ensure_lead_program_loaded()
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
-- answer riding mag.run_result): its text when it carries one. The encode
-- fallback excludes transcript_delta — the conversation record is history's
-- concern, never part of the answer text.
local function mag_result_text(result)
  if type(result) ~= "table" then return nil end
  if type(result.text) == "string" and #result.text > 0 then
    return result.text
  end
  if type(result.final_answer) == "string" and #result.final_answer > 0 then
    return result.final_answer
  end
  local bare = {}
  for k, v in pairs(result) do
    if k ~= "transcript_delta" then bare[k] = v end
  end
  local ok, encoded = pcall(json.encode, bare)
  if ok and type(encoded) == "string" then return encoded end
  return nil
end

-- Validate a transcript delta into recordable messages: a non-empty array of
-- role-tagged message tables, or nil. Arrives off the wire (mag.run_result →
-- result.transcript_delta) or from a replayed turn_recorded marker, so the
-- shape is checked once here; an ill-shaped delta falls back to the bare
-- {user, answer} pair rather than corrupting the seed.
local function transcript_messages(delta)
  if type(delta) ~= "table" or #delta == 0 then return nil end
  for i = 1, #delta do
    local m = delta[i]
    if type(m) ~= "table" or type(m.role) ~= "string" or #m.role == 0 then
      return nil
    end
  end
  return delta
end

-- Commit one turn to the canonical conversation history and log the durable
-- `turn_recorded` marker (replayed on /resume to rebuild state.history). Every
-- terminal path that must preserve context routes through here. A completed
-- turn carries the llm's transcript delta — the user task plus every tool
-- exchange plus the final answer, recorded verbatim so the next turn's seed
-- replays what the model saw. A turn without a usable delta (killed/failed,
-- or a program that didn't end in an llm) records the bare `{ user, answer }`
-- pair. Skips empty user_text (a relay/system turn with no message to
-- preserve). The marker carries `messages` (the recorded delta) alongside the
-- `user`/`answer` summary fields session tooling reads.
local function record_turn(run_id, user_text, answer, transcript_delta)
  if type(user_text) ~= "string" or #user_text == 0 then return end
  local messages = transcript_messages(transcript_delta)
  if messages ~= nil then
    for _, m in ipairs(messages) do
      state.history[#state.history + 1] = m
    end
  else
    state.history[#state.history + 1] = { role = "user", content = user_text }
    state.history[#state.history + 1] = { role = "assistant", content = answer }
  end
  emit(nil, {
    kind     = "agentic_loop.turn_recorded",
    run_id   = run_id,
    user     = user_text,
    answer   = answer,
    messages = messages,
  })
end

-- The assistant-slot placeholder for a turn that ended without a real answer.
-- A user-initiated hard kill or the graceful "interrupted by
-- user" settle) records the same honest marker the user saw; any other failure
-- records its error so the next turn can see what broke.
local function aborted_turn_marker(err)
  local text = tostring(err or "")
  if #text == 0 or text:find("interrupt", 1, true) then
    return "[interrupted by user]"
  end
  return "[turn failed: " .. text .. "]"
end

-- Terminal close of the lead's turn-program.
--   completed — the answer already painted into the transcript via the
--     prefix-bound stream (exactly how the lead's final answer renders
--     today); when no stream flowed (a non-streaming provider), the text
--     is appended so the turn is never silently empty. Canonical history
--     gains the turn's transcript delta (bare `{ user, answer }` pair when
--     the result carries none), and a turn_recorded marker rides the bus
--     so /resume can rebuild it.
--   failed — surfaced in chat as a system line, AND the turn is recorded
--     with a placeholder answer so context survives (an interrupted lead
--     turn settles here; without the record the next turn seeds blind).
--   killed — turn aborted (hard kill): no transcript append,
--     but the turn IS recorded with an interrupted placeholder so the
--     user's message survives into the next turn's seed.
local function handle_mag_run_result(body)
  local turn = state.current_turn
  if turn == nil or body.run_id ~= turn.run_id then return end
  local run_id = turn.run_id
  state.current_run_id = nil
  state.current_turn = nil

  if body.status == "completed" then
    local answer = mag_result_text(body.result) or ""
    local delta = type(body.result) == "table" and body.result.transcript_delta or nil
    record_turn(run_id, turn.user_text, answer, delta)
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
    -- A killed turn (hard kill) still preserves context: the
    -- user's message plus an interrupted placeholder land in history so the
    -- next turn is not seeded blind. The transcript already carries the
    -- interrupt notice from cancel_all.
    record_turn(run_id, turn.user_text, aborted_turn_marker(body.error))
    nefor.log.info("agentic-loop: lead turn killed", {
      run_id = run_id, history_len = #state.history,
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
  local err_text = "[lead turn failed] " .. tostring(body.error or body.status or "unknown error")
  emit("nefor-tui", {
    kind = "chat.message.append",
    role = "system",
    text = err_text,
  })
  -- Preserve context: record the turn with a placeholder answer so the user's
  -- message survives into the next turn's seed (an interrupted lead turn
  -- settles here). Without this the next turn seeds blind — the amnesia bug.
  record_turn(run_id, turn.user_text, aborted_turn_marker(body.error))
  nefor.log.warn("agentic-loop: lead turn failed", {
    run_id = run_id, error = body.error, status = body.status,
    history_len = #state.history,
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
  state.deferred_queue     = {}
  state.pending_user_inputs = {}
  state.pending_inputs_projected = 0
  state.pending_steer       = nil
  state.history            = {}
  emit_idle_state("session-ended")
  nefor.log.info("agentic-loop: sessions.session_end → state cleared", {})
end

-- Stream-visible check by chat_id: true for the active lead turn's
-- prefix-scoped kernel chats (so the provider compositor fires the
-- public stream observers for the lead's own deltas — the CLI surface
-- reads those).
local function stream_visible(chat_id)
  local turn = state.current_turn
  if turn ~= nil and type(turn.chat_prefix) == "string"
      and starts_with(chat_id, turn.chat_prefix) then
    return true
  end
  return false
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
function M.stream_visible(chat_id) return stream_visible(chat_id) end

-- Whether a gate correlation id belongs to the lead's ACTIVE turn (ids are
-- `<scope>/cap-N`; the lead's own tool firings carry its turn scope, a
-- dispatched sub-run's carry that run's scope). Public so a tool can route
-- by caller: mag-eval detaches lead-called runs (their results relay through
-- relay_run_completion) and stays blocking for graph agents, which have no
-- relay channel.
function M.lead_scoped_id(id) return lead_scoped_id(id) end

function M.fire_stream_observers(text) fire_stream_observers(text) end
function M.fire_reasoning_observers(text) fire_reasoning_observers(text) end
function M.fire_tool_start_observers(id, name, input) fire_tool_start_observers(id, name, input) end
function M.fire_tool_end_observers(id, output, err) fire_tool_end_observers(id, output, err) end
function M.set_reasoning_effort(provider, effort) set_reasoning_effort(provider, effort) end

function M._teardown_for_session_end() return teardown_for_session_end() end

function M.config() return state.config end

-- The canonical conversation history (read-only view; the recorded
-- transcript per completed turn). Tests and surfaces read it; mutations
-- belong to the turn lifecycle only.
function M.history() return state.history end

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
        or kind == "chat.steer"
        or kind == "chat.reset"
        or kind == "chat.interrupt"
        or kind == "chat.interrupt_all"
        or kind == "chat.model.set"
        or kind == "chat.reasoning.set" then
      return
    end
    -- Canonical-history rebuild: each completed turn logged one
    -- turn_recorded marker; replaying them restores the conversation the
    -- next turn seeds into its llm. A marker carrying the turn's recorded
    -- transcript messages replays them verbatim; an older/delta-less marker
    -- falls back to the bare pair.
    if kind == "agentic_loop.turn_recorded" then
      local messages = transcript_messages(body.messages)
      if messages ~= nil then
        for _, m in ipairs(messages) do
          state.history[#state.history + 1] = m
        end
      elseif type(body.user) == "string" then
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
  if kind == "chat.steer"        then steer_pending_inputs(); return end
  if kind == "chat.interrupt"    then cancel(body.drop_queued == true); return end
  if kind == "chat.interrupt_all" then cancel_all(); return end
  if kind == "chat.model.set" then handle_chat_model_set(body); return end
  if kind == "chat.reasoning.set" then handle_chat_reasoning_set(body); return end

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
      module_roots = nil,
      artifact = nil,
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
    state.pending_inputs_projected = 0
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
