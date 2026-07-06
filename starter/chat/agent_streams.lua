-- Per-actor stream capture for MAG runs. Every actor's provider traffic
-- already broadcasts on the bus with a scoped chat handle
-- (`<scope>/<actor_id>@r<round>`); this module stores the scope→run map
-- (from `mag.run_started`) and folds `chat.stream.*` /
-- `chat.message.append` events into bounded per-actor ring buffers plus
-- a last-activity stamp. Pure state mutators in the run_panel style —
-- the chat reducer taps `record` BEFORE the transcript's foreign-chat
-- guard, so capture never changes what the lead transcript renders.
--
-- State fields owned here (both fed by existing broadcasts):
--   scope_to_run  = { ["r3"] = run_id }
--   agent_streams = {
--     [run_id] = {
--       [actor_id] = {
--         entries = { { kind, text, round, at_ms, role? }, ... },
--         last_activity_ms   = ...,
--         last_activity_kind = ...,
--       },
--     },
--   }

local common = require("chat.common")
local shallow_merge = common.shallow_merge

local M = {}

-- Ring-buffer bounds: at most MAX_ENTRIES entries per actor; a
-- coalescing entry stops growing at MAX_ENTRY_TEXT and the next delta
-- starts a fresh entry, so eviction keeps total memory bounded.
M.MAX_ENTRIES    = 200
M.MAX_ENTRY_TEXT = 4000

-- A WORKING (busy) actor whose last stream event is older than this reads
-- as "possibly stuck" — busy-and-silent. The sidebar leaf row and the
-- agent-view header both style the stale indicator as a warning past it.
-- An idle actor (between activations) never warns: silence is that
-- state's normal shape.
M.STALE_MS = 30000

-- Parse a kernel-scoped chat handle: `<scope>/<actor_id>@r<round>`.
-- Actor ids may contain dots (`explorer.llm`) but never `@`, so the
-- round splits on the LAST `@`. Returns (scope, actor_id, round) or nil
-- for unscoped / non-kernel handles.
function M.parse_chat_id(chat_id)
  if type(chat_id) ~= "string" then return nil end
  local scope, rest = chat_id:match("^([^/]+)/(.+)$")
  if scope == nil then return nil end
  local actor_id, round = rest:match("^(.+)@r(%d+)$")
  if actor_id == nil then return nil end
  return scope, actor_id, tonumber(round)
end

-- Store the scope a `mag.run_started` broadcast carries so later
-- chat_id parses resolve to the run.
function M.set_scope(state, scope, run_id)
  if type(scope) ~= "string" or #scope == 0 then return state end
  if type(run_id) ~= "string" or #run_id == 0 then return state end
  local prev = state.scope_to_run or {}
  if prev[scope] == run_id then return state end
  local next_map = {}
  for k, v in pairs(prev) do next_map[k] = v end
  next_map[scope] = run_id
  return shallow_merge(state, { scope_to_run = next_map })
end

-- Activity kind → buffered entry kind. End-of-stream kinds stamp
-- activity only (no entry); the timeline already carries their text.
local ENTRY_KIND = {
  delta           = "delta",
  reasoning_delta = "reasoning",
  message         = "message",
}

-- Consecutive same-round deltas of the same kind merge into one entry
-- so a token stream doesn't burn one ring slot per token.
local COALESCE = { delta = true, reasoning = true }

-- A global monotonic capture sequence stamps every NEW buffer entry
-- (stream or tool) as it is recorded. The composite (whole-agent /
-- whole-run) view merges member buffers by this seq — buffers are
-- append-ordered per actor, so a single sort on capture order
-- reconstructs the interleaved arrival timeline without needing clock
-- precision. Coalesced deltas keep their original seq (their timeline
-- position is stable); end-of-stream stamps mint no entry, no seq.
local function bump_seq(state)
  return (state.capture_seq or 0) + 1
end

-- Attribute a scope-prefixed correlation id (`r<K>/…`) to a run. Tool
-- gate envelopes carry `id = <scope>/cap-N`; the leading scope token is
-- the same one `mag.run_started` bound. Returns run_id or nil.
local function run_of_scoped_id(state, id)
  if type(id) ~= "string" then return nil end
  local scope = id:match("^([^/]+)/")
  if scope == nil then return nil end
  return (state.scope_to_run or {})[scope]
end

-- Write one actor's next buffer back into the streams map, returning the
-- full new state. `patch` overrides last_activity fields.
local function put_actor(state, run_id, actor_id, entries, now_ms, kind, extra_patch)
  local prev_streams = state.agent_streams or {}
  local prev_run     = prev_streams[run_id] or {}
  local next_actor = {
    entries            = entries,
    last_activity_ms   = now_ms,
    last_activity_kind = kind,
  }
  local next_run = {}
  for k, v in pairs(prev_run) do next_run[k] = v end
  next_run[actor_id] = next_actor
  local next_streams = {}
  for k, v in pairs(prev_streams) do next_streams[k] = v end
  next_streams[run_id] = next_run
  local patch = { agent_streams = next_streams }
  for k, v in pairs(extra_patch or {}) do patch[k] = v end
  return shallow_merge(state, patch)
end

-- Fold one observed stream event into the actor's buffer. `kind` is the
-- activity kind ("delta", "reasoning_delta", "stream_end",
-- "reasoning_end", "message"). Unparseable / unknown-scope chat ids are
-- ignored — this taps a broadcast bus, not a validated feed.
function M.record(state, chat_id, kind, text, now_ms, role)
  local scope, actor_id, round = M.parse_chat_id(chat_id)
  if scope == nil then return state end
  local run_id = (state.scope_to_run or {})[scope]
  if run_id == nil then return state end

  local prev_run   = (state.agent_streams or {})[run_id] or {}
  local prev_actor = prev_run[actor_id] or { entries = {} }

  local entries = prev_actor.entries or {}
  local new_seq = nil
  local entry_kind = ENTRY_KIND[kind]
  if entry_kind ~= nil and type(text) == "string" and #text > 0 then
    local next_entries = {}
    for i = 1, #entries do next_entries[i] = entries[i] end
    local last = next_entries[#next_entries]
    if last ~= nil and COALESCE[entry_kind] and last.kind == entry_kind
        and last.round == round and #last.text < M.MAX_ENTRY_TEXT then
      next_entries[#next_entries] = {
        kind = entry_kind, text = last.text .. text,
        round = round, at_ms = last.at_ms, role = last.role,
        seq = last.seq,
      }
    else
      new_seq = bump_seq(state)
      next_entries[#next_entries + 1] = {
        kind = entry_kind, text = text,
        round = round, at_ms = now_ms, role = role, seq = new_seq,
      }
      while #next_entries > M.MAX_ENTRIES do table.remove(next_entries, 1) end
    end
    entries = next_entries
  end

  return put_actor(state, run_id, actor_id, entries, now_ms, kind,
    new_seq and { capture_seq = new_seq } or nil)
end

-- ── tool-event capture (per emitting actor) ───────────────────────────
--
-- The tool gate broadcasts `tool-gate.tool.invoke { id, from, name, args }`
-- and the correlated `tool.result { id, output|error }`. `from` is the
-- emitting actor's plain address (e.g. `scout.run-tool`) — so tool events
-- attribute straight to an actor buffer, grouped under the same namespace
-- prefix the panel groups members by. The result carries no `from`, so it
-- correlates back by the shared scoped `id`.

-- Short, single-line preview of an invoke's args. Prefers the common
-- primary field (a command / path / query) over a full dump.
local function short_args(args)
  if type(args) == "string" then return args end
  if type(args) ~= "table" then return "" end
  for _, k in ipairs({ "command", "cmd", "path", "file", "query", "text", "pattern", "name" }) do
    if type(args[k]) == "string" then return args[k] end
  end
  local parts = {}
  for k, v in pairs(args) do
    if type(v) == "string" or type(v) == "number" then
      parts[#parts + 1] = tostring(k) .. "=" .. tostring(v)
    end
    if #parts >= 2 then break end
  end
  return table.concat(parts, " ")
end

local function clip(s, n)
  s = tostring(s or ""):gsub("%s+", " ")
  if #s > n then return s:sub(1, n - 1) .. "…" end
  return s
end
M.clip = clip
M.short_args = short_args

-- Turn a tool.result body into a short status + output preview.
local function result_preview(output, err)
  if err ~= nil and err ~= "" then
    if type(err) == "string" then return "error", err end
    return "error", short_args(err)
  end
  if type(output) == "string" then return "ok", output end
  if type(output) == "table" then
    for _, k in ipairs({ "text", "content", "stdout", "output", "result" }) do
      if type(output[k]) == "string" then return "ok", output[k] end
    end
    return "ok", short_args(output)
  end
  return "ok", ""
end

-- Buffer a tool invoke onto the emitting actor's stream. New pending
-- entry, seq-stamped like any other; the matching result updates it in
-- place (see record_tool_result).
function M.record_tool_invoke(state, from, id, name, args, now_ms)
  if type(from) ~= "string" or #from == 0 then return state end
  local run_id = run_of_scoped_id(state, id)
  if run_id == nil then return state end

  local prev_run   = (state.agent_streams or {})[run_id] or {}
  local prev_actor = prev_run[from] or { entries = {} }
  local entries = {}
  for i = 1, #(prev_actor.entries or {}) do entries[i] = prev_actor.entries[i] end

  local seq = bump_seq(state)
  entries[#entries + 1] = {
    kind         = "tool",
    id           = id,
    name         = name or "?",
    args_preview = clip(short_args(args), 60),
    status       = "pending",
    at_ms        = now_ms,
    seq          = seq,
  }
  while #entries > M.MAX_ENTRIES do table.remove(entries, 1) end
  return put_actor(state, run_id, from, entries, now_ms, "tool.invoke",
    { capture_seq = seq })
end

-- Correlate a tool.result back to its pending invoke by the shared scoped
-- `id` and finalise that entry (status + output preview) in place.
function M.record_tool_result(state, id, output, err, now_ms)
  local run_id = run_of_scoped_id(state, id)
  if run_id == nil then return state end
  local run = (state.agent_streams or {})[run_id]
  if run == nil then return state end

  local status, preview = result_preview(output, err)
  for actor_id, actor in pairs(run) do
    for i = #(actor.entries or {}), 1, -1 do
      local e = actor.entries[i]
      if e.kind == "tool" and e.id == id and e.status == "pending" then
        local entries = {}
        for j = 1, #actor.entries do entries[j] = actor.entries[j] end
        entries[i] = {
          kind = "tool", id = e.id, name = e.name,
          args_preview = e.args_preview, status = status,
          output_preview = clip(preview, 80), at_ms = e.at_ms, seq = e.seq,
        }
        return put_actor(state, run_id, actor_id, entries, now_ms,
          "tool.result", nil)
      end
    end
  end
  return state
end

-- Drop buffers + scope bindings for runs no longer in `runs` (the
-- pruned run map) so capture state follows the existing linger/prune
-- lifecycle instead of leaking across runs.
function M.prune(state, runs)
  local live = runs or {}
  local streams   = state.agent_streams
  local scope_map = state.scope_to_run
  local stale_stream, stale_scope = false, false
  for run_id in pairs(streams or {}) do
    if live[run_id] == nil then stale_stream = true break end
  end
  for _, run_id in pairs(scope_map or {}) do
    if live[run_id] == nil then stale_scope = true break end
  end
  if not stale_stream and not stale_scope then return state end
  local patch = {}
  if stale_stream then
    local kept = {}
    for run_id, v in pairs(streams) do
      if live[run_id] ~= nil then kept[run_id] = v end
    end
    patch.agent_streams = kept
  end
  if stale_scope then
    local kept = {}
    for scope, run_id in pairs(scope_map) do
      if live[run_id] ~= nil then kept[scope] = run_id end
    end
    patch.scope_to_run = kept
  end
  return shallow_merge(state, patch)
end

function M.actor_stream(state, run_id, actor_id)
  local streams = state.agent_streams or {}
  local run = streams[run_id]
  if run == nil then return nil end
  return run[actor_id]
end

-- Merge the buffers of every actor in `run_id` for which `predicate(actor_id)`
-- holds into one chronological list, ordered by capture seq. Each element is
-- `{ actor_id, entry }` so the composite view can attribute a line to the
-- member that produced it. Also returns the newest last-activity ms across
-- the selected members (for the composite header's diagnostic).
function M.merged_entries(state, run_id, predicate)
  local run = (state.agent_streams or {})[run_id]
  if run == nil then return {}, nil, nil end
  local items = {}
  local last_ms, last_kind = nil, nil
  for actor_id, actor in pairs(run) do
    if predicate == nil or predicate(actor_id) then
      for _, e in ipairs(actor.entries or {}) do
        items[#items + 1] = { actor_id = actor_id, entry = e }
      end
      if actor.last_activity_ms ~= nil
          and (last_ms == nil or actor.last_activity_ms > last_ms) then
        last_ms   = actor.last_activity_ms
        last_kind = actor.last_activity_kind
      end
    end
  end
  table.sort(items, function(a, b)
    local sa, sb = a.entry.seq or 0, b.entry.seq or 0
    if sa ~= sb then return sa < sb end
    return (a.actor_id or "") < (b.actor_id or "")
  end)
  return items, last_ms, last_kind
end

return M
