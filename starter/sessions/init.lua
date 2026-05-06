-- starter/sessions/init.lua — session-management actor.
--
-- ## Layout
--
-- This plugin is a folder. `init.lua` (this file) is the production
-- actor — `require("sessions")` returns it. `test.lua` is a sibling
-- module exporting test escape hatches; `require("sessions.test")` is
-- imported only by tests, never by production code.
--
-- ## Shape
--
-- Returns the actor table — `{ name, receive_msg, send_msg, ... }` —
-- which the starter registers via `actor.spawn(require("sessions"))`.
-- The actor runtime calls `receive_msg(entry)` for every wire envelope;
-- the module's own code calls `send_msg(...)` to emit.
--
-- ## What this module owns
--
-- All session knowledge — id, on-disk jsonl path, persistence of bus
-- traffic, in-process resume. Filtering replay traffic is the consumer
-- plugin's concern; sessions broadcasts every recorded step-origin
-- entry to its original target on resume.
--
-- ## Public bus protocol
--
-- Four control events. None are persisted (anything whose body.kind
-- starts with `sessions.` is dropped from disk).
--
--   sessions.session_start { session_id, from_resume? }
--   sessions.session_end   { session_id }
--   sessions.resume_done   { session_id, replayed }
--     ↑ emitted
--
--   sessions.resume_request { session_id }
--   sessions.new_request    { }
--     ↑ consumed
--
-- ## On-disk path
--
-- Resolved once at module load:
--   1. `$NEFOR_DATA_HOME` — test override.
--   2. `$XDG_DATA_HOME/nefor` — standard XDG.
--   3. `$HOME/.local/share/nefor` — XDG default fallback.
-- Path: `<root>/sessions/<id>.jsonl`. Parent dir is created on init.

local json = nefor.json

-- ------------------------------------------------------------------
-- module state — single table; mutations are explicit (state.x = ...)
-- ------------------------------------------------------------------

local state = {
  ---@type string|nil
  current_session_id   = nil,
  ---@type string|nil
  current_session_path = nil,
  ---@type file*|nil
  current_session_file = nil,

  -- True when the active session has nothing worth keeping; flipped
  -- false when (a) the file is opened on a non-empty pre-existing
  -- file, or (b) any envelope is persisted to it.
  should_prune_session = true,

  -- Re-entrance guard for resume. A bus consumer that emits during a
  -- resume can circle back through `sessions.resume_request` /
  -- `sessions.new_request` and re-trigger; without this guard the
  -- result is a runaway cascade. Phase 2 closes the cascade class
  -- structurally (porting agentic_workflow); the guard goes away then.
  resume_in_progress = false,

  initialised = false,

  -- Synchronous resume-phase hook registry. Phase 1 compat for
  -- agentic_workflow's `replay_mode` flip; goes away in Phase 2.
  resume_phase_hooks = {
    session_end   = {},
    session_start = {},
    resume_done   = {},
  },
}

-- ------------------------------------------------------------------
-- on-disk paths — computed once at module load
-- ------------------------------------------------------------------

---@return string|nil
local function compute_data_root()
  local override = os.getenv("NEFOR_DATA_HOME")
  if override and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME")
  if not home or home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local DATA_ROOT    = compute_data_root()
local SESSIONS_DIR = DATA_ROOT and (DATA_ROOT .. "/sessions") or nil

---@param id string
---@return string|nil
local function session_path_for(id)
  return SESSIONS_DIR and (SESSIONS_DIR .. "/" .. id .. ".jsonl") or nil
end

---@param path string
local function ensure_dir(path)
  os.execute(string.format("mkdir -p %q 2>/dev/null", path))
end

-- ------------------------------------------------------------------
-- helpers — uuid, file io
-- ------------------------------------------------------------------

do
  local addr_byte = string.byte(tostring({}):sub(-2, -2)) or 0
  math.randomseed((os.time() * 1000) + math.floor((os.clock() or 0) * 1e6) + addr_byte)
end

-- Pure-Lua UUID v4. RFC 4122 version + variant nibbles in the right slots.
---@return string
local function uuid_v4()
  local function hex(n) return string.format("%x", math.random(0, n)) end
  local function hex_n(n)
    local out = {}
    for _ = 1, n do out[#out + 1] = hex(15) end
    return table.concat(out)
  end
  return string.format(
    "%s-%s-4%s-%s%s-%s",
    hex_n(8), hex_n(4), hex_n(3),
    ({ "8", "9", "a", "b" })[math.random(1, 4)],
    hex_n(3), hex_n(12)
  )
end

---@param path string
---@param session_id string
---@return file*|nil, string|nil
local function open_session_file(path, session_id)
  local probe = io.open(path, "r")
  local has_content = false
  if probe then
    has_content = (probe:read("*l") or "") ~= ""
    probe:close()
  end

  local fh, err = io.open(path, "a")
  if not fh then return nil, tostring(err) end

  if not has_content then
    local header_line = json.encode({
      _session   = true,
      session_id = session_id,
      started_at = nefor.engine.now(),
    })
    fh:write(header_line)
    fh:write("\n")
    fh:flush()
  end

  return fh, nil, has_content
end

local function close_session_file()
  if state.current_session_file then
    pcall(state.current_session_file.close, state.current_session_file)
    state.current_session_file = nil
  end
end

local function close_and_prune_if_empty()
  close_session_file()
  if state.should_prune_session and state.current_session_path then
    pcall(os.remove, state.current_session_path)
  end
  state.should_prune_session = true
end

-- ------------------------------------------------------------------
-- send_msg — translate plugin-internal output to wire envelope, emit
-- ------------------------------------------------------------------

---@param internal table
local function send_msg(internal)
  if internal.kind == "control" then
    local body = { kind = internal.event }
    if internal.extra then
      for k, v in pairs(internal.extra) do body[k] = v end
    end
    nefor.engine.send(json.encode({
      type = "event",
      from = "sessions",
      ts   = nefor.engine.now(),
      body = body,
    }))
  elseif internal.kind == "replay_envelope" then
    nefor.engine.send(internal.payload, internal.target)
  end
end

-- ------------------------------------------------------------------
-- persistence — write each non-control envelope verbatim to jsonl
-- ------------------------------------------------------------------

---@param entry { ts: string?, origin: string?, target: string?, payload: string }
local function persist_envelope(entry)
  if not state.current_session_file then return end

  -- Drop sessions.* control events. They're starter-internal lifecycle
  -- signals, not session content.
  local ok, decoded = pcall(json.decode, entry.payload)
  if ok and type(decoded) == "table" and type(decoded.body) == "table" then
    local kind = decoded.body.kind
    if type(kind) == "string" and kind:sub(1, 9) == "sessions." then return end
  end

  local row = {
    ts      = entry.ts or nefor.engine.now(),
    origin  = entry.origin or "unknown",
    payload = entry.payload,
  }
  if entry.target then row.target = entry.target end

  state.current_session_file:write(json.encode(row))
  state.current_session_file:write("\n")
  state.current_session_file:flush()

  state.should_prune_session = false
end

-- ------------------------------------------------------------------
-- resume — re-broadcast every step-origin entry to its original target
-- ------------------------------------------------------------------

---@param path string
---@return integer count
local function replay_jsonl(path)
  local fh = io.open(path, "r")
  if not fh then return 0 end

  local count = 0
  for line in fh:lines() do
    -- Skip the header (cheap substring; full parse only on entries).
    if line:sub(1, 12) ~= [[{"_session":]] then
      local ok, decoded = pcall(json.decode, line)
      -- Replay every step-origin entry — those are the dispatch hook's
      -- outbound emissions. Both targeted (`target` set) and broadcast
      -- (`target` nil) shapes round-trip via send_msg's
      -- replay_envelope path: nefor.engine.send(payload, target?) with
      -- target=nil broadcasts and a string targets one peer. Plugin-
      -- origin entries are inputs — we don't re-emit them, peers re-
      -- announce themselves on connect.
      if ok and decoded.origin == "step" then
        send_msg({
          kind    = "replay_envelope",
          payload = decoded.payload,
          target  = decoded.target,  -- may be nil (broadcast)
        })
        count = count + 1
      end
    end
  end
  fh:close()
  return count
end

---@param phase "session_end"|"session_start"|"resume_done"
---@param session_id string|nil
local function fire_resume_phase(phase, session_id)
  for _, fn in ipairs(state.resume_phase_hooks[phase]) do
    local ok, err = pcall(fn, session_id)
    if not ok and nefor.log then
      nefor.log.error("sessions: resume-phase hook raised", {
        phase = phase, error = tostring(err),
      })
    end
  end
end

-- ------------------------------------------------------------------
-- resume + new
-- ------------------------------------------------------------------

---@param target_session_id string
local function do_resume(target_session_id)
  if state.resume_in_progress then
    if nefor.log then
      nefor.log.warn("sessions.resume: re-entrant call dropped", {
        requested        = target_session_id,
        currently_active = state.current_session_id,
      })
    end
    return
  end
  if state.current_session_id == target_session_id then
    return  -- no-op resume
  end
  state.resume_in_progress = true

  -- 1. Announce end of outgoing session.
  fire_resume_phase("session_end", state.current_session_id)
  send_msg({ kind = "control", event = "sessions.session_end",
             extra = { session_id = state.current_session_id } })

  -- 2. Swap state.
  close_and_prune_if_empty()
  local new_path = session_path_for(target_session_id)
  if new_path then ensure_dir(SESSIONS_DIR) end
  state.current_session_id   = target_session_id
  state.current_session_path = new_path

  if new_path then
    local fh, err, had_content = open_session_file(new_path, target_session_id)
    if not fh and nefor.log then
      nefor.log.error("sessions.resume: failed to open session file", {
        path = new_path, error = err,
      })
    end
    state.current_session_file = fh
    state.should_prune_session = not had_content
  end

  -- 3. Announce start of incoming session BEFORE replay.
  fire_resume_phase("session_start", target_session_id)
  send_msg({ kind = "control", event = "sessions.session_start",
             extra = { session_id = target_session_id, from_resume = true } })

  -- 4. Replay.
  local replayed = new_path and replay_jsonl(new_path) or 0

  -- 5. Coalesced "we're back" signal.
  fire_resume_phase("resume_done", target_session_id)
  send_msg({ kind = "control", event = "sessions.resume_done",
             extra = { session_id = target_session_id, replayed = replayed } })

  state.resume_in_progress = false
end

local function do_new()
  do_resume(uuid_v4())
end

-- ------------------------------------------------------------------
-- shutdown
-- ------------------------------------------------------------------

local function do_shutdown()
  send_msg({ kind = "control", event = "sessions.session_end",
             extra = { session_id = state.current_session_id } })
  close_and_prune_if_empty()
end

-- ------------------------------------------------------------------
-- init (boot path)
-- ------------------------------------------------------------------

local function do_init()
  if state.initialised then
    if nefor.log then
      nefor.log.warn("sessions.init: already initialised; ignoring", {
        session_id = state.current_session_id,
      })
    end
    return state.current_session_id
  end

  local id = uuid_v4()
  local path = session_path_for(id)
  if path then ensure_dir(SESSIONS_DIR) end

  state.current_session_id   = id
  state.current_session_path = path
  if path then
    local fh, err, had_content = open_session_file(path, id)
    if not fh and nefor.log then
      nefor.log.error("sessions.init: failed to open session file", {
        path = path, error = err,
      })
    end
    state.current_session_file = fh
    state.should_prune_session = not had_content
  end

  state.initialised = true

  send_msg({ kind = "control", event = "sessions.session_start",
             extra = { session_id = id } })

  if nefor.log then
    nefor.log.info("sessions.init: session opened", { session_id = id, path = path })
  end

  return id
end

-- ------------------------------------------------------------------
-- receive_msg — runtime-driven inbound handler
-- ------------------------------------------------------------------

---@param entry { ts: string?, origin: string?, target: string?, payload: string }
local function receive_msg(entry)
  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end

  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local kind = decoded.body.kind

  -- Lifecycle: synthesized engine shutdown.
  if kind == "engine.shutdown" then
    do_shutdown()
    return
  end

  -- Resume to a specific session id.
  if kind == "sessions.resume_request" then
    local target = decoded.body.session_id
    if type(target) == "string" and target ~= "" then
      do_resume(target)
    end
    return
  end

  -- Mint a fresh session.
  if kind == "sessions.new_request" then
    do_new()
    return
  end

  -- Drop sessions.* control events from persistence.
  if type(kind) == "string" and kind:sub(1, 9) == "sessions." then return end

  -- Everything else: persist.
  persist_envelope(entry)
end

-- ------------------------------------------------------------------
-- on_resume_phase — synchronous hook registry (Phase 1 compat)
-- ------------------------------------------------------------------

---@param phase "session_end"|"session_start"|"resume_done"
---@param fn function
local function on_resume_phase(phase, fn)
  local list = state.resume_phase_hooks[phase]
  list[#list + 1] = fn
end

-- ------------------------------------------------------------------
-- module table — actor contract + public Lua API
-- ------------------------------------------------------------------

return {
  -- actor contract
  name        = "sessions",
  receive_msg = receive_msg,
  send_msg    = send_msg,

  -- public Lua API
  init             = do_init,
  resume           = do_resume,
  new              = do_new,
  current_id       = function() return state.current_session_id end,
  current_path     = function() return state.current_session_path end,
  on_resume_phase  = on_resume_phase,
  -- handle_shutdown is a no-op now: the actor.lua runtime synthesizes
  -- an engine.shutdown wire envelope and our receive_msg handles it.
  handle_shutdown  = function() end,

  -- Internal handle for `sessions/test.lua` only. Production code must
  -- not reach for this; it exists so the test surface can live in a
  -- separate module without duplicating private helpers.
  _internals = {
    state              = state,
    persist_envelope   = persist_envelope,
    do_resume          = do_resume,
    do_new             = do_new,
    do_shutdown        = do_shutdown,
    uuid_v4            = uuid_v4,
    compute_data_root  = compute_data_root,
    reset_state        = function()
      close_session_file()
      state.current_session_id    = nil
      state.current_session_path  = nil
      state.should_prune_session  = true
      state.resume_in_progress    = false
      state.initialised           = false
      state.resume_phase_hooks    = {
        session_end   = {},
        session_start = {},
        resume_done   = {},
      }
    end,
  },
}
