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

-- A running actor whose last stream event is older than this reads as
-- "possibly stuck" — the sidebar leaf row and the agent-view header
-- both style the idle indicator as a warning past it.
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

-- Fold one observed stream event into the actor's buffer. `kind` is the
-- activity kind ("delta", "reasoning_delta", "stream_end",
-- "reasoning_end", "message"). Unparseable / unknown-scope chat ids are
-- ignored — this taps a broadcast bus, not a validated feed.
function M.record(state, chat_id, kind, text, now_ms, role)
  local scope, actor_id, round = M.parse_chat_id(chat_id)
  if scope == nil then return state end
  local run_id = (state.scope_to_run or {})[scope]
  if run_id == nil then return state end

  local prev_streams = state.agent_streams or {}
  local prev_run     = prev_streams[run_id] or {}
  local prev_actor   = prev_run[actor_id] or { entries = {} }

  local entries = prev_actor.entries or {}
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
      }
    else
      next_entries[#next_entries + 1] = {
        kind = entry_kind, text = text,
        round = round, at_ms = now_ms, role = role,
      }
      while #next_entries > M.MAX_ENTRIES do table.remove(next_entries, 1) end
    end
    entries = next_entries
  end

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
  return shallow_merge(state, { agent_streams = next_streams })
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

return M
