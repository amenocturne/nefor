-- starter/sessions.lua — Lua-side session management for nefor.
--
-- ## What this module owns
--
-- All session knowledge — id, on-disk jsonl path, persistence of bus
-- traffic, in-process resume — lives in this module. The Rust engine is
-- session-blind: it forwards inbound lines to step, broadcasts events,
-- and exits when asked. It does not know what a session is. Session
-- continuity (boot, shutdown, resume) is composed in Lua over the bus.
--
-- ## Public bus protocol
--
-- The library emits and consumes four control events. They are NEVER
-- persisted to the jsonl log (the persistence hook drops anything whose
-- kind starts with `sessions.`).
--
--   sessions.session_start { session_id }
--     Emitted on:
--       * app boot, after `sessions.init()` mints a fresh id;
--       * resume, AFTER the prior `sessions.session_end` has fired and
--         the new id is in place but BEFORE replay begins.
--     Subscribers: per-plugin handlers (B3) that need to flush state
--     when the active session flips. Payload `session_id` is the new
--     active session id.
--
--   sessions.session_end { session_id }
--     Emitted on:
--       * shutdown (synchronously, inside the engine shutdown grace);
--       * resume, BEFORE `sessions.session_start` of the new id, so
--         per-plugin handlers can teardown state owned by the outgoing
--         session before the new one is announced.
--     Payload `session_id` is the session being ended.
--
--   sessions.resume_done { session_id }
--     Emitted AFTER `sessions.resume` has finished replaying the
--     target jsonl onto the bus. UI surfaces (chat etc.) use this as
--     the "we're back, repaint final state" signal — they should NOT
--     react to individual replayed envelopes if they need a single
--     coalesced redraw.
--
--   sessions.resume_request { session_id }
--     Consumed (subscribed via `nefor.bus.on_event`). The session
--     picker (out of scope for this module — see B3) emits this when
--     the user picks a target session. The library responds by calling
--     `sessions.resume(session_id)`. The request itself is a control
--     event and is NOT persisted.
--
-- ## Public Lua API
--
--   sessions.init()
--     Boot path. Mints a fresh UUID v4, computes the on-disk jsonl
--     path, ensures parent dirs exist, sets module-local state, and
--     emits `sessions.session_start { session_id }`. Idempotent: a
--     second call is a no-op (logs a warning and returns the existing
--     id). Call from `init.lua` once during boot, AFTER the bus is
--     ready (i.e. after `nefor.bus` is installed by the engine, which
--     it is by the time `init.lua` runs).
--
--   sessions.resume(target_session_id)
--     Switch the active session to `target_session_id`. Sequence:
--       1. Emit `sessions.session_end { session_id = current }`.
--       2. Swap state to the target id + path.
--       3. Emit `sessions.session_start { session_id = new }`.
--       4. Read target jsonl line-by-line, decode each as an NCP
--          envelope, re-emit each on the bus via `nefor.engine.send`
--          (broadcast to every connected peer). Self-origin entries
--          are skipped (a plugin must never see its own replay).
--          Engine-origin entries are skipped (private to the
--          translation layer).
--       5. Emit `sessions.resume_done { session_id = new }`.
--     Errors: if the target file is missing or unreadable, the swap
--     still happens (we still own the new id) but replay is skipped
--     and a warning logs. The caller's `sessions.session_start`
--     subscribers will fire on a fresh-feeling session. This matches
--     the user's expectation that picking a session always succeeds
--     in some defined way rather than half-rolling-back.
--
--   sessions.on_resume_phase(phase, fn)
--     Register a synchronous hook fired inside `M.resume()` immediately
--     BEFORE the corresponding `emit_control` broadcast. Phases:
--     "session_end" / "session_start" / "resume_done". Use this when a
--     module's state must flip in lockstep with the resume sequence —
--     specifically, when racing the bus subscriber's next-tick fire would
--     leak stale traffic. The canonical caller is agentic_workflow.lua's
--     `replay_mode = true` flag, which must be set before reasoner-graph
--     replay envelopes ship.
--     Errors raised inside a hook are caught + logged; subsequent hooks
--     still fire.
--
--   sessions.handle_shutdown()
--     Wire the shutdown handler. Subscribes to the engine-internal
--     lifecycle event `"shutdown"` (`nefor.events.on("shutdown", ...)`)
--     — that's the bus the engine emits on inside its cooperative-
--     shutdown grace, before connections close. The handler emits
--     `sessions.session_end { session_id = current }` synchronously so
--     per-plugin handlers can observe it before their stdin closes,
--     then flushes + closes the jsonl file. Call from `init.lua` once
--     after `sessions.init()`.
--
--     The lifecycle bus (`nefor.events.on`) is distinct from the NCP
--     plugin bus (`nefor.bus.on_event`): plugin bus carries protocol
--     traffic; lifecycle bus carries engine-internal signals
--     (startup / shutdown / tick — see `crates/nefor/src/events`).
--     Shutdown is a lifecycle signal, hence `nefor.events`.
--
-- ## Persistence rules
--
-- A bus subscription appended on init (`nefor.bus.on_event("*", ...)`)
-- fires per envelope routed through the broker. The handler receives a
-- log-entry table — `{ ts, origin, target?, payload }` — where `payload`
-- is the raw JSON wire string. We write that table verbatim as a jsonl
-- line, mirroring what the Rust `SessionWriter` writes so chat.lua's
-- existing picker keeps working:
--   {"ts":"<iso>","origin":"<plugin-or-step>","payload":"<wire-json>"}
--   {"ts":"<iso>","origin":"<plugin-or-step>","target":"<peer>","payload":"..."}
--
-- DROPPED envelopes (not persisted):
--   * any inner-body kind starting with `sessions.` (the four control
--     events). We re-decode the payload to inspect kind because Lua
--     table iteration order isn't guaranteed and JSON encoding may
--     place "kind" anywhere — substring sniff is unreliable.
--   * envelopes whose payload doesn't decode or has no `body.kind`.
--
-- The first line written to a fresh session file is a `{_session:true,
-- session_id, started_at}` header matching the Rust SessionHeader
-- (post session-blind refactor) so the picker's parsing in chat.lua
-- continues to recognise the file.
--
-- ## On-disk path
--
-- Resolution order, first hit wins:
--   1. `$NEFOR_DATA_HOME` — test override; existing convention used by
--      chat.lua's picker and the chat_test harness.
--   2. `$XDG_DATA_HOME/nefor` — standard XDG.
--   3. `$HOME/.local/share/nefor` — XDG default fallback.
-- Path: `<root>/sessions/<id>.jsonl`. Parent dir is created on init.
--
-- ## Why a Lua module instead of a plugin
--
-- Persistence is a tap on the bus that needs to fire on every envelope
-- without adding a stdin/stdout round-trip per event. Same reasoning as
-- `resume.lua` (the legacy module): a Lua module in the engine's VM is
-- a function call, not an NCP round-trip.
--
-- ## Relationship to legacy resume.lua (removed)
--
-- This module replaces the legacy `starter/resume.lua` + `saved_log`
-- code path. Per-plugin replay filtering is no longer in a separate
-- registry — the live agentic_workflow transforms react to the
-- `sessions.session_start` / `session_end` / `resume_done` lifecycle
-- envelopes via `sessions.on_resume_phase` (synchronous, lockstep with
-- the resume sequence) and `nefor.bus.on_event` (next-tick broadcast),
-- and own their own state-flush / replay-mode flags. ncp.dispatch and
-- the engine no longer carry a saved_log argument.

local M = {}

local json = nefor.json

-- ------------------------------------------------------------------
-- module-local state
-- ------------------------------------------------------------------

-- The active session id (UUID v4 string) and its jsonl path. nil until
-- `sessions.init()` has run.
local current_session_id   = nil
local current_session_path = nil

-- File handle for the active session's jsonl, kept open across writes
-- so we don't pay an open() per envelope. Reopened on resume. Closed
-- on session_end / shutdown.
local current_session_file = nil

-- Count of `chat.input.submit` envelopes persisted since the active file
-- was opened. Used at close time to delete sessions that show as empty
-- in the picker so it doesn't fill up with "started a chat, said
-- nothing, quit" ghost entries. We count submits specifically (not all
-- non-control envelopes) because boot handshake (`combinators.hello`,
-- `chat.model.set_ack`, etc.) lands many envelopes before the user has
-- typed anything — counting those would defeat the prune. The picker's
-- own preview filter uses the same `chat.input.submit` signal, so this
-- keeps the two in lockstep: a session the picker shows as `(no
-- submits)` is exactly a session this counter calls empty. Reset on
-- every open (init and resume swap).
local submits_written = 0

-- Subscription bookkeeping so tests can reset between scenarios.
local persistence_installed = false
local resume_listener_installed = false
local shutdown_listener_installed = false

-- Synchronous resume-phase hook registry. Modules whose state must flip
-- IN LOCKSTEP with the resume sequence (rather than one broker-tick later
-- via `nefor.bus.on_event`) register here. Each list fires once per
-- resume, in registration order, INSIDE `M.resume()` immediately before
-- the corresponding `emit_control` broadcast.
--
-- Why a Lua-local registry instead of the bus: bus subscribers fire on
-- the broker's NEXT tick (the broker captures `new_entries` before step
-- runs, so step's outbound `nefor.engine.send` sits unsubscribed-to until
-- the next inbound line). Between the session_start broadcast and the
-- agentic_workflow handler firing, the replay loop here can already have
-- pushed reasoner-graph envelopes onto the wire — the handler that flips
-- `replay_mode = true` runs too late to gate them. Synchronous hooks run
-- BEFORE the broadcast, so any state these modules own is already in the
-- replay-correct shape before the first replay envelope ships.
--
-- Keys: "session_end", "session_start", "resume_done". Values: array of
-- `function(session_id) ... end` callbacks. Errors in a callback are
-- caught + logged; subsequent callbacks still run.
local resume_phase_hooks = {
  session_end   = {},
  session_start = {},
  resume_done   = {},
}

-- ------------------------------------------------------------------
-- helpers — uuid, paths, file io
-- ------------------------------------------------------------------

-- Seed math.random once per module load. Same approach as
-- agentic_workflow.lua: mix os.time, os.clock, and a fresh-table address
-- for entropy across processes spawned in the same wall-clock second.
do
  local addr_byte = string.byte(tostring({}):sub(-2, -2)) or 0
  math.randomseed((os.time() * 1000) + math.floor((os.clock() or 0) * 1e6) + addr_byte)
end

-- Pure-Lua UUID v4. Matches Rust's `uuid::Uuid::new_v4().to_string()`
-- format (lowercase hex, 8-4-4-4-12 with version=4 / variant=10xx
-- nibbles in the right slots). The Rust engine accepts any version via
-- `uuid::Uuid::parse_str` so structural validity is what counts.
local function uuid_v4()
  local function hex(n) return string.format("%x", math.random(0, n)) end
  local function hex_n(n)
    local out = {}
    for _ = 1, n do out[#out + 1] = hex(15) end
    return table.concat(out)
  end
  -- Version 4: 13th hex digit is "4". Variant: 17th hex digit is one of
  -- 8/9/a/b. These constraints come from RFC 4122.
  return string.format(
    "%s-%s-4%s-%s%s-%s",
    hex_n(8),
    hex_n(4),
    hex_n(3),
    ({ "8", "9", "a", "b" })[math.random(1, 4)],
    hex_n(3),
    hex_n(12)
  )
end

-- Resolve the data root. NEFOR_DATA_HOME wins (test override + existing
-- chat.lua convention); then XDG_DATA_HOME/nefor; then
-- $HOME/.local/share/nefor.
local function data_root()
  local override = os.getenv("NEFOR_DATA_HOME")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function sessions_dir()
  local root = data_root()
  if root == nil then return nil end
  return root .. "/sessions"
end

local function session_path_for(id)
  local dir = sessions_dir()
  if dir == nil then return nil end
  return dir .. "/" .. id .. ".jsonl"
end

-- Best-effort `mkdir -p` via shell. Lua's stdlib has no portable
-- recursive-mkdir; chat.lua already uses io.popen for directory ops, so
-- the dependency is established. Returns true on success or if the dir
-- already exists; false on failure.
local function ensure_dir(path)
  if path == nil then return false end
  -- POSIX `mkdir -p` is idempotent and creates parents. Quote the path
  -- for spaces in $HOME (Library/Application Support, etc.). 2>/dev/null
  -- swallows benign "exists" noise.
  local cmd = string.format("mkdir -p %q 2>/dev/null", path)
  local ok = os.execute(cmd)
  -- Lua 5.4 returns true | nil on os.execute; older returns 0 | code.
  return ok == true or ok == 0 or ok == 0.0
end

-- Open the active session's jsonl file in append mode and write the
-- header line. Idempotent on the file: if the file already exists with
-- content, we append; if it's fresh, we write the header first. Returns
-- a file handle or nil + error string.
local function open_session_file(path, session_id)
  if path == nil then return nil, "no path" end
  -- Detect whether the file is already populated (e.g. opened across a
  -- crash/restart with the same id). We don't want to write a second
  -- header line.
  local probe = io.open(path, "r")
  local has_header = false
  if probe ~= nil then
    local first = probe:read("*l")
    has_header = first ~= nil and #first > 0
    probe:close()
  end

  local fh, err = io.open(path, "a")
  if fh == nil then return nil, tostring(err) end

  if not has_header then
    -- Header shape mirrors the Rust SessionHeader (post session-blind
    -- engine refactor): `{ _session, session_id, started_at }`.
    -- chat.lua's session picker reads back `started_at` for display.
    local header = {
      _session   = true,
      session_id = session_id,
      started_at = nefor.engine.now(),
    }
    local ok, header_line = pcall(json.encode, header)
    if ok then
      fh:write(header_line)
      fh:write("\n")
      fh:flush()
    end
  end

  return fh, nil
end

local function close_session_file()
  if current_session_file ~= nil then
    pcall(function() current_session_file:close() end)
    current_session_file = nil
  end
end

-- Close the active session file and, if no real user activity was
-- recorded, delete the jsonl so it doesn't show up as an empty stub in
-- the picker. "Real activity" means at least one `chat.input.submit`
-- envelope was persisted — see `submits_written` doc above. Resets the
-- counter so the next open starts clean.
local function close_and_prune_if_empty()
  close_session_file()
  if submits_written == 0 and current_session_path ~= nil then
    pcall(os.remove, current_session_path)
  end
  submits_written = 0
end

-- ------------------------------------------------------------------
-- bus emission helpers
-- ------------------------------------------------------------------

-- Emit a control event on the bus. Broadcast to every connected peer.
-- `body` becomes `body` of the envelope; we synthesise the wrapper. We
-- bypass NCP transforms (using `nefor.engine.send` directly per peer)
-- because control events are starter-internal — no plugin should
-- intercept them with from_plugin/to_plugin transforms.
local function emit_control(kind, extra)
  local body = { kind = kind }
  if type(extra) == "table" then
    for k, v in pairs(extra) do body[k] = v end
  end
  local ok, payload = pcall(json.encode, {
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = body,
  })
  if not ok then
    -- Same defence as agentic_workflow.lua's emit() — log and skip.
    if nefor.log and nefor.log.error then
      nefor.log.error("sessions: failed to encode control event", {
        kind  = kind,
        error = tostring(payload),
      })
    end
    return
  end
  -- Engine.send uses broadcast when the second arg is omitted; that
  -- delivers to every connected peer. The starter's ncp.lua uses
  -- per-peer iteration to skip the sender, but for control events we
  -- ARE the engine — every peer should see them.
  nefor.engine.send(payload)
end

-- ------------------------------------------------------------------
-- persistence hook
-- ------------------------------------------------------------------

-- Called per envelope routed through the bus. The dispatcher hands us a
-- log entry — `{ ts, origin, target?, payload }` — where `payload` is
-- the raw JSON wire string of the NCP envelope. We peek inside `payload`
-- only enough to decide whether to drop it (control events) and then
-- write the original log-entry shape verbatim to the jsonl file. That
-- shape mirrors what the Rust SessionWriter writes, so chat.lua's
-- session picker (which reads jsonl headers + lines) keeps working.
--
-- Synchronous — Lua's `io` is buffered and flushed on each line, which
-- keeps a crash from losing more than the in-flight envelope. Async
-- isn't required at the volumes involved (a chat turn is hundreds of
-- small lines, not thousands per second).
local function persist_envelope(entry)
  if current_session_file == nil then return end
  if type(entry) ~= "table" then return end
  local payload = entry.payload
  if type(payload) ~= "string" or #payload == 0 then return end

  -- Filter out control events. The dispatcher already pre-parsed kind to
  -- route us, but we don't get it as a parameter — so decode the payload
  -- and inspect body.kind. Decoding is cheaper than the alternative
  -- (substring-sniffing across all possible JSON encodings of "kind"
  -- with arbitrary whitespace and key-order — Lua table iteration order
  -- isn't guaranteed and serde_json preserves whatever pairs() gave it).
  local decode_ok, decoded = pcall(json.decode, payload)
  if not decode_ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local kind = decoded.body.kind
  if type(kind) == "string" and kind:sub(1, 9) == "sessions." then return end

  -- Write the engine-side log-entry shape verbatim. `target` is omitted
  -- when nil to match the SessionWriter's `skip_serializing_if` rule.
  local row = {
    ts      = entry.ts or nefor.engine.now(),
    origin  = entry.origin or "unknown",
    payload = payload,
  }
  if entry.target ~= nil then row.target = entry.target end

  local encode_ok, row_line = pcall(json.encode, row)
  if not encode_ok then return end

  -- Best-effort write — a transient I/O error shouldn't crash step.
  local write_ok = pcall(function()
    current_session_file:write(row_line)
    current_session_file:write("\n")
    current_session_file:flush()
  end)
  if write_ok and kind == "chat.input.submit" then
    submits_written = submits_written + 1
  end
end

-- Install the persistence subscription. Uses a wildcard pattern so
-- every kind reaches the hook (the hook itself filters). Idempotent
-- via the module-local guard so re-invoking init() during tests
-- doesn't double-register.
local function install_persistence()
  if persistence_installed then return end
  -- Pattern "*" → KindPattern::Prefix("") in the engine's bus binding,
  -- which matches every string. Same trick agentic_cli.lua uses to
  -- print every envelope.
  if nefor.bus and nefor.bus.on_event then
    nefor.bus.on_event("*", persist_envelope)
    persistence_installed = true
  end
end

-- ------------------------------------------------------------------
-- resume
-- ------------------------------------------------------------------

-- Plugins that accept replay traffic. Replay's job is to rebuild the
-- UI surface's view of the past — not to re-drive provider HTTP calls,
-- re-dispatch graphs, or re-fire tool gates. Listing the replay target
-- explicitly keeps the side-effecting plugins blind to replay.
--
-- Adding more receivers (e.g. a future logging surface) is one entry.
local REPLAY_TARGETS = { ["nefor-tui"] = true }

-- Replay each jsonl entry to the UI surface(s) listed in REPLAY_TARGETS.
--
-- We replay STEP-ORIGIN entries that targeted a replay-receiver. Step
-- entries are the broker's per-peer post-transform fan-out: each
-- plugin's `from_plugin` ran on the inbound raw line, the result was
-- egress-transformed via `to_plugin` per peer, and the broker enqueued
-- one step entry per peer. Replaying step entries to their original
-- target lands the EXACT envelope each peer received — no transform
-- re-application needed.
--
-- Plugin-origin entries (the raw inbound lines) are NOT replayed:
--   * they're the source of step entries above — replaying both would
--     double-deliver to peers that read the post-transform shape.
--   * the raw kind (e.g. `ollama.stream.delta`) is NOT what any peer
--     received post-transform; routing it to nefor-tui without the
--     `for_provider.from_plugin` translation leaves the TUI looking
--     for `chat.stream.delta` — the transcript stays empty.
--
-- Engine-origin entries are skipped — `engine.*` kinds are private to
-- the translation layer and never belong on the bus as broadcast
-- events.
--
-- Side-effecting plugins (providers, reasoner-graph, tool-gate, etc.)
-- are explicitly NOT replay targets: they already cleared state on
-- `sessions.session_end` and re-receiving past commands would replay
-- side effects (provider HTTP calls, graph dispatches, tool invokes).
--
-- Streaming deltas that are subsumed by their finalizers are dropped
-- so resume is INSTANT, not "watch the whole conversation re-stream
-- in real time". `chat.stream.end` carries the final assistant text,
-- so dropping the per-token `chat.stream.delta` events is safe — the
-- finalizer will build the entry from `text` directly. Reasoning
-- deltas are KEPT because `chat.stream.reasoning_end` carries no
-- payload (just `duration_ms`); the reasoning text only exists in
-- the deltas and dropping them would erase the reasoning view.
-- Reasoning deltas are typically short relative to assistant text,
-- so this still gives a fast replay.
local REPLAY_DROP_KINDS = {
  ["chat.stream.delta"] = true,
}

-- Sniff the body kind out of a raw NCP wire payload. Substring search
-- is good enough — replay is bulk and a full JSON decode per line on a
-- 50k-event session is wasteful when the answer is one of two strings.
-- False positives are bounded to envelopes whose body literally
-- contains `"kind":"chat.stream.delta"` somewhere — which is the kind
-- itself, the only legitimate match we care about.
local function payload_has_drop_kind(payload)
  for kind, _ in pairs(REPLAY_DROP_KINDS) do
    if payload:find('"kind":"' .. kind .. '"', 1, true) then
      return true
    end
  end
  return false
end

local function replay_jsonl(path)
  if path == nil then return 0 end
  local fh = io.open(path, "r")
  if fh == nil then return 0 end
  local count = 0
  for line in fh:lines() do
    -- Skip the header. Cheap substring check; full parse only on
    -- entries.
    if line:sub(1, 12) == [[{"_session":]] then
      -- header line, skip
    else
      local ok, decoded = pcall(json.decode, line)
      if ok and type(decoded) == "table" and type(decoded.payload) == "string" then
        if decoded.origin == "step"
           and type(decoded.target) == "string"
           and REPLAY_TARGETS[decoded.target]
           and not payload_has_drop_kind(decoded.payload) then
          nefor.engine.send(decoded.payload, decoded.target)
          count = count + 1
        end
      end
    end
  end
  fh:close()
  return count
end

-- Fire every callback registered for `phase` synchronously, in
-- registration order. `pcall` so a faulty hook doesn't break the resume
-- sequence. Returns nothing — the side effects are what matters.
local function fire_resume_phase(phase, session_id)
  local list = resume_phase_hooks[phase]
  if list == nil then return end
  for _, fn in ipairs(list) do
    local ok, err = pcall(fn, session_id)
    if not ok and nefor.log and nefor.log.error then
      nefor.log.error("sessions: resume-phase hook raised", {
        phase = phase,
        error = tostring(err),
      })
    end
  end
end

-- Register a synchronous hook for one of the three resume phases.
-- Called inside `M.resume()` BEFORE the corresponding `emit_control`
-- broadcast, so module state owned outside sessions.lua can flip in
-- lockstep with the resume sequence rather than racing the broker's
-- next-tick bus dispatch. See `resume_phase_hooks` doc above for the
-- "why" (one-tick-leak fix).
--
-- `phase` ∈ {"session_end", "session_start", "resume_done"}. Unknown
-- phases raise — typo-resistant.
function M.on_resume_phase(phase, fn)
  if type(phase) ~= "string" or resume_phase_hooks[phase] == nil then
    error(string.format(
      "sessions.on_resume_phase: phase must be one of session_end / session_start / resume_done; got %s",
      tostring(phase)
    ), 2)
  end
  if type(fn) ~= "function" then
    error("sessions.on_resume_phase: fn must be a function", 2)
  end
  local list = resume_phase_hooks[phase]
  list[#list + 1] = fn
end

function M.resume(target_session_id)
  if type(target_session_id) ~= "string" or target_session_id == "" then
    if nefor.log and nefor.log.error then
      nefor.log.error("sessions.resume: target_session_id must be a non-empty string", {
        got = type(target_session_id),
      })
    end
    return
  end
  if current_session_id == target_session_id then
    -- No-op resume: already on this session. Don't fire the cycle.
    if nefor.log and nefor.log.info then
      nefor.log.info("sessions.resume: already active", { session_id = target_session_id })
    end
    return
  end

  -- 1. Announce end of outgoing session.
  --    Synchronous hooks fire FIRST so module state (e.g. in-flight
  --    bookkeeping that needs to clear before the bus broadcast) is in
  --    the correct shape by the time peers see session_end.
  fire_resume_phase("session_end", current_session_id)
  emit_control("sessions.session_end", { session_id = current_session_id })

  -- 2. Swap state.
  close_and_prune_if_empty()
  local new_path = session_path_for(target_session_id)
  if new_path ~= nil then
    ensure_dir(sessions_dir())
  end
  current_session_id   = target_session_id
  current_session_path = new_path
  if new_path ~= nil then
    local fh, err = open_session_file(new_path, target_session_id)
    if fh == nil and nefor.log and nefor.log.error then
      nefor.log.error("sessions.resume: failed to open session file", {
        path  = new_path,
        error = err,
      })
    end
    current_session_file = fh
  end

  -- 3. Announce start of incoming session — BEFORE replay so handlers
  -- can teardown stale state and prepare to accept replayed envelopes.
  --    Synchronous hooks fire BEFORE the broadcast: this is the
  --    one-tick-leak fix. agentic_workflow's `replay_mode = true` flag
  --    must be set before any envelope produced by replay_jsonl below
  --    routes through reasoner-graph's `from_plugin`. The bus-event
  --    subscriber for `sessions.session_start` only runs on the next
  --    broker tick (see `resume_phase_hooks` doc), which is already too
  --    late.
  fire_resume_phase("session_start", target_session_id)
  emit_control("sessions.session_start", { session_id = target_session_id })

  -- 4. Replay. Errors here don't roll the swap back — see module doc.
  local replayed = replay_jsonl(new_path)

  -- 5. Coalesced "we're back" signal.
  --    Synchronous hooks fire FIRST so listeners can lift any replay-
  --    related gates before the broadcast goes out (symmetric with
  --    session_start).
  fire_resume_phase("resume_done", target_session_id)
  emit_control("sessions.resume_done", {
    session_id = target_session_id,
    replayed   = replayed,
  })
end

-- ------------------------------------------------------------------
-- bus subscriptions: resume_request + shutdown
-- ------------------------------------------------------------------

-- The dispatcher delivers a log-entry table `{ts, origin, target?, payload}`
-- where `payload` is the raw envelope JSON. We decode just enough to
-- pull `body.session_id` and call `resume`.
local function on_resume_request(entry)
  if type(entry) ~= "table" or type(entry.payload) ~= "string" then return end
  local ok, decoded = pcall(json.decode, entry.payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local target = decoded.body.session_id
  if type(target) ~= "string" or target == "" then return end
  M.resume(target)
end

local function install_resume_listener()
  if resume_listener_installed then return end
  if nefor.bus and nefor.bus.on_event then
    nefor.bus.on_event("sessions.resume_request", on_resume_request)
    resume_listener_installed = true
  end
end

-- Engine shutdown handler. Synchronous: we emit the session_end inside
-- the engine's cooperative-shutdown grace so per-plugin handlers can
-- still observe it before connections close. We also flush + close the
-- jsonl file here so a crash after shutdown-emit but before process
-- exit doesn't lose the tail.
--
-- Called by `nefor.events` with a payload Lua value (typically nil for
-- lifecycle signals); we don't read it. Param name `_payload` documents
-- the shape and underscore-prefix marks intentional non-use.
local function on_engine_shutdown(_payload)
  emit_control("sessions.session_end", { session_id = current_session_id })
  close_and_prune_if_empty()
end

function M.handle_shutdown()
  if shutdown_listener_installed then return end
  -- Engine-internal lifecycle bus — distinct from the NCP plugin bus
  -- (`nefor.bus.on_event`). The engine emits "shutdown" via its
  -- internal `EventBus` inside the cooperative-shutdown grace window
  -- (see `crates/nefor/src/events/mod.rs`'s `SHUTDOWN` constant); we
  -- subscribe via `nefor.events.on` so our `session_end` emission
  -- happens BEFORE plugin connections close.
  if nefor.events and nefor.events.on then
    nefor.events.on("shutdown", on_engine_shutdown)
    shutdown_listener_installed = true
  end
end

-- ------------------------------------------------------------------
-- init
-- ------------------------------------------------------------------

function M.init()
  if current_session_id ~= nil then
    if nefor.log and nefor.log.warn then
      nefor.log.warn("sessions.init: already initialised; ignoring", {
        session_id = current_session_id,
      })
    end
    return current_session_id
  end

  local id = uuid_v4()
  local path = session_path_for(id)
  if path ~= nil then
    ensure_dir(sessions_dir())
  end

  current_session_id   = id
  current_session_path = path
  if path ~= nil then
    local fh, err = open_session_file(path, id)
    if fh == nil and nefor.log and nefor.log.error then
      nefor.log.error("sessions.init: failed to open session file", {
        path  = path,
        error = err,
      })
    end
    current_session_file = fh
  end

  install_persistence()
  install_resume_listener()

  emit_control("sessions.session_start", { session_id = id })

  if nefor.log and nefor.log.info then
    nefor.log.info("sessions.init: session opened", {
      session_id = id,
      path       = path,
    })
  end

  return id
end

-- ------------------------------------------------------------------
-- introspection (no public stability guarantees; for tests + diagnostics)
-- ------------------------------------------------------------------

function M.current_id()
  return current_session_id
end

function M.current_path()
  return current_session_path
end

-- Test-only: clear all module state and bus-listener guards. The bus
-- subscriptions registered with the engine are NOT removed (the engine
-- has no off_event), but tests run in a fresh Lua VM per case so this
-- doesn't matter.
function M._reset()
  close_session_file()
  submits_written      = 0
  current_session_id   = nil
  current_session_path = nil
  persistence_installed     = false
  resume_listener_installed = false
  shutdown_listener_installed = false
  resume_phase_hooks = {
    session_end   = {},
    session_start = {},
    resume_done   = {},
  }
end

-- Exposed for tests so they can drive the persistence hook directly
-- without having to pump the bus.
M._persist_envelope = persist_envelope
M._on_resume_request = on_resume_request
M._on_engine_shutdown = on_engine_shutdown
M._uuid_v4 = uuid_v4
M._data_root = data_root

return M
