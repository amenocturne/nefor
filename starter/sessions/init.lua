-- starter/sessions/init.lua — session-management actor.
--
-- Owns session knowledge: id, on-disk jsonl path, persistence of bus
-- traffic, in-process resume. Filtering replay traffic is the consumer
-- plugin's concern; sessions broadcasts every recorded step-origin
-- entry to its original target on resume.
--
-- The test escape-hatch surface (`require("sessions.test")`) lives
-- under `tests/lua/sessions/test.lua` and is on the package.path only
-- in the Rust test harness.
--
-- ## Public bus protocol
--
-- Six control events. None are persisted (anything whose body.kind
-- starts with `sessions.` is dropped from disk).
--
--   sessions.session_start { session_id, from_resume? }
--   sessions.session_end   { session_id }
--   sessions.replay.start  { session_id, count }
--   sessions.replay.end    { session_id }
--   sessions.resume_done   { session_id, replayed }
--     ↑ emitted
--
--   sessions.resume_request { session_id }
--   sessions.new_request    { }
--     ↑ consumed
--
-- ## Replay window
--
-- `replay.start` / `replay.end` frame the burst of replayed envelopes.
-- The runtime gate (in `ncp.lua`) suppresses bus→Rust forwarding inside
-- the window so Rust plugins never see replayed traffic; pure-Lua
-- actors get the replay normally via `nefor.bus.on_event` and rebuild
-- their state. Sessions itself ALSO drops persistence between the
-- markers — pure-Lua actors may emit derived envelopes during state
-- rebuild, and persisting those would duplicate state on next resume.
-- The persist rule is "live traffic only."
--
-- ## On-disk path
--
-- Resolved once at module load from `nefor.fs.data_root()` — the
-- engine's canonical resolved data directory (CLI flag >
-- `NEFOR_DATA_DIR` env var > `XDG_DATA_HOME/nefor`).
-- Path: `<root>/sessions/<id>.jsonl`. Parent dir is created on init.

local json           = nefor.json
local history_replay = require("core.history_replay")

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

  initialised = false,

  -- Replay-window flag — flipped by receive_msg when our own
  -- `sessions.replay.start` / `sessions.replay.end` markers re-enter
  -- through the bus. While true, `persist_envelope` drops everything:
  -- pure-Lua actors may emit derived envelopes during replay-driven
  -- state rebuild, and persisting those would duplicate state on the
  -- next resume. The rule is "persist live traffic only."
  in_replay_window = false,
}

---@return string|nil
local function compute_data_root()
  return nefor.fs.data_root()
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
  -- Best-effort recursive mkdir via the Rust binding. Idempotent on
  -- EEXIST; any permission error surfaces on the next io.open the
  -- writer attempts (the return value here is intentionally ignored —
  -- the next write is the source of truth on success).
  nefor.fs.mkdir_p(path)
end

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
    local header = {
      _session   = true,
      session_id = session_id,
      started_at = nefor.engine.now(),
    }
    local cwd = nefor.fs.getcwd()
    if cwd then header.cwd = cwd end
    fh:write(json.encode(header))
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

-- send_msg — translate plugin-internal output to wire envelope, emit.
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

-- Persistence — write each non-control envelope verbatim to jsonl.
---@param entry { ts: string?, origin: string?, target: string?, payload: string }
local function persist_envelope(entry)
  if not state.current_session_file then return end

  -- Drop everything inside the replay window. Pure-Lua actors process
  -- replayed envelopes (via bus.on_event) and may emit derived ones —
  -- persisting those would duplicate state on next resume. The window
  -- is bounded by `sessions.replay.start` / `sessions.replay.end`,
  -- which receive_msg uses to toggle `in_replay_window`. The markers
  -- themselves fall through to the `sessions.*` filter below.
  if state.in_replay_window then return end

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

-- Resume — re-broadcast every step-origin entry to its original target.
--
-- Pre-pass: count the step-origin entries that would be replayed. Used
-- to populate `sessions.replay.start { count }`. Cheaper than buffering
-- the whole file: one extra read of the JSONL with a substring check
-- before per-line decode (decode is unavoidable to filter step-origin).
---@param path string
---@return table|nil header
local function read_session_header(path)
  local fh = io.open(path, "r")
  if not fh then return nil end
  local first = fh:read("*l")
  fh:close()
  if not first or first == "" then return nil end
  local ok, decoded = pcall(json.decode, first)
  if ok and type(decoded) == "table" and decoded._session then
    return decoded
  end
  return nil
end

---@param path string
---@return integer count
local function count_replay_entries(path)
  local fh = io.open(path, "r")
  if not fh then return 0 end
  local count = 0
  for line in fh:lines() do
    if line:sub(1, 12) ~= [[{"_session":]] then
      local ok, decoded = pcall(json.decode, line)
      if ok and type(decoded) == "table" and decoded.origin == "step" then
        count = count + 1
      end
    end
  end
  fh:close()
  return count
end

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
      if ok and type(decoded) == "table" and decoded.origin == "step" then
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

---@param target_session_id string
local function do_resume(target_session_id)
  -- Same-id resume is a re-load, not a no-op. Chat.lua's `/resume`
  -- and picker handlers locally clear the transcript BEFORE emitting
  -- `sessions.resume_request` (the imminent replay is expected to
  -- repaint), so an early-return here would leave the user staring
  -- at an empty chat. Cycling the full lifecycle replays the on-disk
  -- log against the (already-cleared) chat surface and rebuilds the
  -- transcript exactly the way a cross-session resume does. close +
  -- reopen of the same path is safe in append mode; the file's prior
  -- traffic is what `replay_jsonl` reads, and `should_prune_session`
  -- stays false when the file has content (so no prune happens).
  --
  -- 1. Announce end of outgoing session. Cold-start `--session` resume
  -- has no outgoing session yet.
  if state.current_session_id then
    send_msg({ kind = "control", event = "sessions.session_end",
               extra = { session_id = state.current_session_id } })
  end

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

  -- 2b. Restore the session's working directory. chdir before replay
  -- so any tool calls replayed into live plugins run in the original
  -- cwd. Falls back silently when the path no longer exists — the
  -- engine stays in whatever cwd it already has.
  if new_path then
    local header = read_session_header(new_path)
    if header and type(header.cwd) == "string" and header.cwd ~= "" then
      local result = nefor.fs.chdir(header.cwd)
      if result.ok then
        if nefor.log then
          nefor.log.info("sessions.resume: chdir", { cwd = header.cwd })
        end
      elseif nefor.log then
        nefor.log.warn("sessions.resume: chdir failed, staying in current dir", {
          target = header.cwd, error = result.error,
        })
      end
    end
  end

  -- 3. Announce start of incoming session BEFORE replay.
  send_msg({ kind = "control", event = "sessions.session_start",
             extra = { session_id = target_session_id, from_resume = true } })

  -- 4. Replay framed by start/end markers. Per-wrapper replay-skip
  -- (core.history_replay) suppresses bus→peer side effects inside the
  -- window; pure-Lua actors get the replay normally and rebuild
  -- state. Sessions' own persistence path also drops envelopes inside
  -- the window — see `in_replay_window` flag toggled in receive_msg.
  -- The window flag's `to_plugin`-side toggle is owned by ncp.dispatch
  -- (it sees the framing markers in entry order and flips the flag
  -- inline before calling `to_plugin` for each entry — bus.on_event
  -- subscribers fire too late for the same batch).
  local total = new_path and count_replay_entries(new_path) or 0
  local replayed = 0
  local replay_started = false
  local replay_ended = false
  history_replay.set(true)
  state.in_replay_window = true
  local ok, err = pcall(function()
    send_msg({ kind = "control", event = "sessions.replay.start",
               extra = { session_id = target_session_id, count = total } })
    replay_started = true
    replayed = new_path and replay_jsonl(new_path) or 0
    send_msg({ kind = "control", event = "sessions.replay.end",
               extra = { session_id = target_session_id } })
    replay_ended = true
  end)
  if replay_started and not replay_ended then
    pcall(function()
      send_msg({ kind = "control", event = "sessions.replay.end",
                 extra = { session_id = target_session_id } })
    end)
  end
  history_replay.set(false)
  state.in_replay_window = false
  if not ok and nefor.log then
    nefor.log.error("sessions.resume: replay failed", {
      session_id = target_session_id,
      error      = tostring(err),
    })
  end

  -- 5. Coalesced "we're back" signal.
  send_msg({ kind = "control", event = "sessions.resume_done",
             extra = { session_id = target_session_id, replayed = replayed } })
end

local function do_new()
  do_resume(uuid_v4())
end

local function do_shutdown()
  send_msg({ kind = "control", event = "sessions.session_end",
             extra = { session_id = state.current_session_id } })
  close_and_prune_if_empty()
end

---@param resume_id string|nil
local function do_init(resume_id)
  if state.initialised then
    if nefor.log then
      nefor.log.warn("sessions.init: already initialised; ignoring", {
        session_id = state.current_session_id,
      })
    end
    return state.current_session_id
  end

  state.initialised = true

  if resume_id and resume_id ~= "" then
    do_resume(resume_id)
    return resume_id
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

  send_msg({ kind = "control", event = "sessions.session_start",
             extra = { session_id = id } })

  if nefor.log then
    nefor.log.info("sessions.init: session opened", { session_id = id, path = path })
  end

  return id
end

-- receive_msg — runtime-driven inbound handler.
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

  -- Replay-window markers — flip the persistence-skip flag. The
  -- marker emissions are sessions's own (via send_msg in do_resume);
  -- they round-trip through the bus and arrive here on a later tick,
  -- which is exactly when the persistence handler needs the flag set
  -- to drop derived emissions from pure-Lua actors processing the
  -- replay. Markers themselves are not persisted (sessions.* filter
  -- below).
  if kind == "sessions.replay.start" then
    state.in_replay_window = true
    return
  end
  if kind == "sessions.replay.end" then
    state.in_replay_window = false
    return
  end

  -- Drop sessions.* control events from persistence.
  if type(kind) == "string" and kind:sub(1, 9) == "sessions." then return end

  -- Everything else: persist.
  persist_envelope(entry)
end

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
  -- handle_shutdown is a no-op now: the actor.lua runtime synthesizes
  -- an engine.shutdown wire envelope and our receive_msg handles it.
  handle_shutdown  = function() end,

  -- Internal handle for the test escape-hatch module only. Production
  -- code must not reach for this; it exists so the test surface can
  -- live in a separate module without duplicating private helpers.
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
      state.initialised           = false
      state.in_replay_window      = false
    end,
  },
}
