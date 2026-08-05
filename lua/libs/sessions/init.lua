-- lua/libs/sessions/init.lua — session-management actor.
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
--   sessions.resume_loading { session_id }
--   sessions.replay.start   { session_id, count }
--   sessions.replay.progress { session_id, replayed, total }
--   sessions.replay.end     { session_id }
--   sessions.resume_done    { session_id, replayed }
--     ↑ emitted
--
--   sessions.resume_request { session_id }
--   sessions.new_request    { }
--     ↑ consumed
--
-- ## Replay window
--
-- Replay uses short `replay.start` / `replay.end` frames around each bounded
-- chunk. The broker may dispatch live traffic between chunks without marking
-- it as replay. Within a chunk, the frame and replayed entries are appended
-- synchronously and retain exact positional provenance.
--
-- ## On-disk path
--
-- Resolved once at module load from `nefor.fs.data_root()` — the
-- engine's canonical resolved data directory (CLI flag >
-- `NEFOR_DATA_DIR` env var > `XDG_DATA_HOME/nefor`).
-- Path: `<root>/sessions/<id>.jsonl`. Parent dir is created on init.

local json = nefor.json
local replay_window = require("core.history_replay")

local state = {
  ---@type string|nil
  current_session_id   = nil,
  ---@type string|nil
  current_session_path = nil,
  ---@type file*|nil
  current_session_file = nil,

  -- True when the active session has nothing worth keeping; flipped
  -- false when a real user submit is persisted. Startup-only sessions
  -- never open a file, so crashes/restarts do not leave empty logs.
  should_prune_session = true,

  initialised = false,

  -- Replay-window flag — flipped by receive_msg when our own
  -- `sessions.replay.start` / `sessions.replay.end` markers re-enter
  -- through the bus. While true, `persist_envelope` drops everything:
  -- pure-Lua actors may emit derived envelopes during replay-driven
  -- state rebuild, and persisting those would duplicate state on the
  -- next resume. The rule is "persist live traffic only."
  in_replay_window = false,

  -- Monotonic owner token for cooperative replay continuations. Session
  -- switches and shutdown invalidate queued work before touching files.
  replay_generation = 0,
  ---@type string|nil
  replay_session_id = nil,
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

local function session_file_exists(path)
  local fh = io.open(path, "r")
  if fh then
    fh:close()
    return true
  end
  return false
end

local function ensure_current_session_file()
  if state.current_session_file then return state.current_session_file end
  if not state.current_session_path then return nil end
  if SESSIONS_DIR then ensure_dir(SESSIONS_DIR) end
  local fh, err, had_content =
      open_session_file(state.current_session_path, state.current_session_id)
  if not fh then
    if nefor.log then
      nefor.log.error("sessions: failed to open session file", {
        path = state.current_session_path, error = err,
      })
    end
    return nil
  end
  state.current_session_file = fh
  state.should_prune_session = not had_content
  return fh
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
    if type(kind) == "string" and kind:sub(1, 13) == "conversation." then
      local canonical = kind == "conversation.fact.recorded"
        and decoded.from == "conversation-manager"
        and decoded.body.duplicate ~= true
      if not canonical then return end
    end
  end

  local is_user_submit = ok
      and type(decoded) == "table"
      and type(decoded.body) == "table"
      and decoded.body.kind == "chat.input.submit"

  if not state.current_session_file and not is_user_submit then return end
  local fh = ensure_current_session_file()
  if not fh then return end

  local row = {
    ts      = entry.ts or nefor.engine.now(),
    origin  = entry.origin or "unknown",
    payload = entry.payload,
  }
  if entry.target then row.target = entry.target end

  fh:write(json.encode(row))
  fh:write("\n")
  fh:flush()

  if is_user_submit then
    state.should_prune_session = false
  end
end

-- Resume is a one-pass cooperative scan. Progress is file-byte progress,
-- measured from the reader cursor, so no synchronous counting pre-pass is
-- needed and malformed/plugin-origin rows still advance the denominator.
local REPLAY_CHUNK_BYTES = 256 * 1024
local REPLAY_CHUNK_ENTRIES = 64
local MAX_PROGRESS_UPDATES = 100

local function file_size(fh)
  local start = fh:seek()
  local total = fh:seek("end")
  if start == nil or total == nil or fh:seek("set", start) == nil then return nil end
  return total
end

local function cancel_replay()
  state.replay_session_id = nil
  state.replay_generation = state.replay_generation + 1
  replay_window.set(false)
end

---@param path string|nil
---@param session_id string
---@param report_progress boolean
---@param generation integer
---@param finish fun(replayed: integer)
local function begin_replay(path, session_id, report_progress, generation, finish)
  local fh = path and io.open(path, "r") or nil
  local total = fh and file_size(fh) or 0
  if total == nil then
    if fh then fh:close() end
    fh = nil
    total = 0
  end

  state.replay_session_id = session_id

  if not fh then
    send_msg({ kind = "control", event = "sessions.replay.start",
               extra = { session_id = session_id, count = total } })
    send_msg({ kind = "control", event = "sessions.replay.end",
               extra = { session_id = session_id } })
    finish(0)
    return
  end

  local progress_step = math.max(1, math.ceil(total / MAX_PROGRESS_UPDATES))
  local next_progress = progress_step
  local delivered = 0

  local function step()
    if generation ~= state.replay_generation then
      fh:close()
      return
    end

    local chunk_start = fh:seek() or 0
    local replayed_entries = 0
    local replay_entries = {}
    while replayed_entries < REPLAY_CHUNK_ENTRIES do
      local cursor = fh:seek() or chunk_start
      if cursor >= total then break end

      local line, read_error = fh:read("*l")
      if line == nil then
        if read_error and nefor.log then
          nefor.log.error("sessions.resume: failed while reading session file", {
            path = path, error = tostring(read_error),
          })
        end
        break
      end

      local after = fh:seek() or cursor
      if after > total then break end

      if line:sub(1, 12) ~= [[{"_session":]] then
        local ok, decoded = pcall(json.decode, line)
        if ok and type(decoded) == "table" and decoded.origin == "step"
            and type(decoded.payload) == "string" then
          replay_entries[#replay_entries + 1] = decoded
          replayed_entries = replayed_entries + 1
        end
      end

      if after - chunk_start >= REPLAY_CHUNK_BYTES then break end
    end

    local replayed = fh:seek() or chunk_start
    local complete = replayed >= total
    local progress_due = report_progress and replayed >= next_progress and replayed < total

    -- Each replay chunk gets its own positional replay frame. Live traffic
    -- can run between continuations without inheriting replay provenance.
    -- The byte budget is checked at record boundaries: replay remains
    -- lossless for valid JSONL, so one unusually large record may make a
    -- single chunk larger than REPLAY_CHUNK_BYTES.
    replay_window.set(true)
    send_msg({ kind = "control", event = "sessions.replay.start",
               extra = { session_id = session_id, count = total } })
    for _, decoded in ipairs(replay_entries) do
      send_msg({ kind = "replay_envelope", payload = decoded.payload,
                 target = decoded.target })
      delivered = delivered + 1
    end
    send_msg({ kind = "control", event = "sessions.replay.end",
               extra = { session_id = session_id } })
    replay_window.set(false)

    if progress_due then
      send_msg({ kind = "control", event = "sessions.replay.progress",
                 extra = { session_id = session_id, replayed = replayed, total = total } })
      next_progress = replayed + progress_step
    end

    if complete then
      fh:close()
      finish(delivered)
      return
    end
    nefor.engine.defer(step)
  end

  nefor.engine.defer(step)
end

---@param target_session_id string
---@param show_loading boolean|nil
local function do_resume(target_session_id, show_loading)
  if show_loading == nil then show_loading = true end
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
  cancel_replay()

  -- 1. Announce end of outgoing session. Cold-start `--session` resume
  -- has no outgoing session yet.
  if state.current_session_id then
    send_msg({ kind = "control", event = "sessions.session_end",
               extra = { session_id = state.current_session_id } })
  end

  -- 2. Swap state.
  close_and_prune_if_empty()
  local new_path = session_path_for(target_session_id)
  state.current_session_id   = target_session_id
  state.current_session_path = new_path

  if new_path and session_file_exists(new_path) then
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
  send_msg({ kind = "control", event = "sessions.session_start",
             extra = { session_id = target_session_id, from_resume = true } })

  -- 4. Replay each bounded chunk in its own start/end frame. This keeps
  -- replay provenance exact while the broker admits live traffic between
  -- continuations: unrelated entries can only land between frames.
  -- Loading starts before the single-pass scan, whose byte total is read
  -- from the already-open file without walking its contents.
  if show_loading then
    send_msg({ kind = "control", event = "sessions.resume_loading",
               extra = { session_id = target_session_id } })
  end
  local generation = state.replay_generation
  begin_replay(new_path, target_session_id, show_loading, generation, function(replayed)
    if generation ~= state.replay_generation then return end
    state.replay_session_id = nil
    send_msg({ kind = "control", event = "sessions.resume_done",
               extra = { session_id = target_session_id, replayed = replayed } })
  end)
end

local function do_new()
  do_resume(uuid_v4())
end

local function do_shutdown()
  cancel_replay()
  state.in_replay_window = false
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
    do_resume(resume_id, false)
    return resume_id
  end

  local id = uuid_v4()
  local path = session_path_for(id)

  state.current_session_id   = id
  state.current_session_path = path
  state.current_session_file = nil
  state.should_prune_session = true

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
      state.replay_generation     = state.replay_generation + 1
      state.replay_session_id     = nil
    end,
  },
}
