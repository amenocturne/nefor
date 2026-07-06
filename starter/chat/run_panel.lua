-- Run-panel sidebar widget: renders the active kernel runs as a column
-- of run-headers + grouped actor rows, and owns the pure-state mutators
-- (mag_run_started, actor_* transitions, mag_run_complete, prune) the
-- chat reducer calls.

local common = require("chat.common")
local agent_streams = require("chat.agent_streams")
local STYLE   = common.STYLE
local CURSOR_ROW_STYLE = common.CURSOR_ROW_STYLE
local shallow_merge = common.shallow_merge

local M = {}

M.LINGER_MS = 2000

local GLYPHS = {
  pending = "○",
  running = "●",
  done    = "✓",
  error   = "✗",
  skipped = "⊘",
  -- A killed actor is a deliberate termination, distinct from a failed
  -- one.
  killed  = "⊗",
  -- MAG member activity states (mag.actor_busy / mag.actor_idle): `working`
  -- is a live activation (ticks its CURRENT activation's elapsed), `idle` is
  -- constructed-between-activations (renders quietly, no timer). The agent
  -- loop's members visibly take turns instead of all ticking the run's
  -- wall clock.
  working = "●",
  idle    = "·",
}
-- Exported for the agent-view popup header (popups.lua) so the status
-- glyph vocabulary stays single-sourced.
M.GLYPHS = GLYPHS

local NODE_STYLE = {
  pending = STYLE.panel_pending,
  running = STYLE.panel_running,
  done    = STYLE.panel_done,
  error   = STYLE.panel_error,
  skipped = STYLE.panel_skipped,
  killed  = STYLE.panel_error,
  working = STYLE.panel_running,
  idle    = STYLE.status_dim,
}

-- Node statuses that count as terminal (drive the done/total counter and
-- freeze the elapsed timer).
local TERMINAL_STATUS = {
  done = true, error = true, skipped = true, killed = true,
}

local function sorted_keys(m)
  local out = {}
  for k in pairs(m) do out[#out + 1] = k end
  table.sort(out)
  return out
end

local function fmt_elapsed_ms(ms)
  if ms == nil then return "" end
  return string.format("%ds", math.floor(ms / 1000))
end
M.fmt_elapsed_ms = fmt_elapsed_ms

-- A completed run lingers in the panel for `LINGER_MS` after its
-- `completed_at_ms` so the user can see the final state. Past that
-- window the run is dropped — visually by the view (this helper),
-- structurally by `prune` on the next reducer dispatch.
local function is_expired(run, now_ms)
  return run.completed_at_ms ~= nil
     and (now_ms - run.completed_at_ms) > M.LINGER_MS
end

function M.prune(runs, now_ms)
  if runs == nil then return {} end
  local pruned = nil
  for run_id, run in pairs(runs) do
    if is_expired(run, now_ms) then
      if pruned == nil then
        pruned = {}
        for k, v in pairs(runs) do pruned[k] = v end
      end
      pruned[run_id] = nil
    end
  end
  return pruned or runs
end

-- `any_active` drives the render-keepalive animation in view.lua, so
-- the engine keeps ticking until the panel is empty. Once every run
-- has completed AND its linger window has closed, the panel is
-- effectively empty (the view filter drops them) — returning false
-- here lets the engine settle. Until then we stay active.
--
-- The wallclock_tick in plugins/nefor-tui/src/main.rs marks the
-- engine dirty every 1s independent of `has_active_animations`, so
-- the linger-window countdown advances even after `any_active` flips
-- false; the next reducer dispatch (or the view-side `is_expired`
-- filter on the next paint) finalises the removal.
function M.any_active(runs, now_ms)
  if type(runs) ~= "table" then return false end
  for _, run in pairs(runs) do
    if not is_expired(run, now_ms or 0) then return true end
  end
  return false
end

-- Elapsed window for an actor row: live-ticking while working (the
-- CURRENT activation only, resetting each mag.actor_busy), frozen at
-- the finish stamp once terminal. Idle carries no window — a
-- between-activations actor shows no timer.
local function node_elapsed_ms(node, now_ms)
  if node.status == "running" then
    return now_ms - (node.started_at_ms or now_ms)
  end
  if node.status == "working" then
    return now_ms - (node.activation_started_at_ms or node.started_at_ms or now_ms)
  end
  if TERMINAL_STATUS[node.status] and node.finished_at_ms ~= nil then
    return node.finished_at_ms - (node.started_at_ms or node.finished_at_ms)
  end
  return nil
end

-- ── MAG namespace grouping (display model) ────────────────────────────
--
-- The run panel collapses a run's actors into top-level namespace
-- groups: an actor id's group is its segment up to the first ".". Group
-- rows aggregate member state; per-member states are retained inside each
-- group (a future expand/collapse toggle reads them straight off the same
-- `run.nodes` store). An undotted id (e.g. `sink`) is its own single-
-- member group.

local function group_of(actor_id)
  return actor_id:match("^([^.]+)") or actor_id
end

-- Aggregate a group's member states into one status. Precedence, highest
-- first: killed (any member killed) → running (any member live — working
-- and idle both count: an agent loop between provider rounds is still a
-- live loop) → done (run finished) → pending (no member ready yet).
-- `killed` outranks `running` so a group with any killed member reads as
-- terminated — the "killed if any member was killed" rule — even while a
-- sibling is still mid-flight.
local LIVE_MEMBER_STATUS = { running = true, working = true, idle = true }

local function group_status(members, run_completed)
  local any_killed, any_running = false, false
  local count = 0
  for _, m in ipairs(members) do
    count = count + 1
    if m.node.status == "killed" then any_killed = true
    elseif LIVE_MEMBER_STATUS[m.node.status] then any_running = true end
  end
  if count == 0 then return "pending" end
  if any_killed then return "killed" end
  if any_running then return "running" end
  if run_completed then return "done" end
  return "pending"
end

-- Build the ordered group model for a MAG run: a list of groups in stable
-- first-appearance order (each group keyed on the earliest spawn `seq` of
-- its members), every group carrying its member nodes plus an aggregated
-- status and elapsed window.
local function build_groups(run)
  local buckets = {}
  local order = {}
  for id, node in pairs(run.nodes or {}) do
    local g = group_of(id)
    local b = buckets[g]
    if b == nil then
      b = { name = g, members = {}, min_seq = math.huge }
      buckets[g] = b
      order[#order + 1] = b
    end
    b.members[#b.members + 1] = { id = id, node = node }
    local seq = node.seq or math.huge
    if seq < b.min_seq then b.min_seq = seq end
  end
  local run_completed = run.completed_at_ms ~= nil
  for _, b in ipairs(order) do
    table.sort(b.members, function(x, y)
      local sx, sy = x.node.seq or math.huge, y.node.seq or math.huge
      if sx ~= sy then return sx < sy end
      return x.id < y.id
    end)
    b.status = group_status(b.members, run_completed)
    local first_start, last_finish
    for _, m in ipairs(b.members) do
      local s = m.node.started_at_ms
      if s and (first_start == nil or s < first_start) then first_start = s end
      local f = m.node.finished_at_ms
      if f and (last_finish == nil or f > last_finish) then last_finish = f end
    end
    b.first_start, b.last_finish = first_start, last_finish
  end
  table.sort(order, function(a, b)
    if a.min_seq ~= b.min_seq then return a.min_seq < b.min_seq end
    return a.name < b.name
  end)
  return order
end

-- Group row. The elapsed window is the WHOLE-RUN clock (first member start →
-- now while live, frozen at the last finish once terminal) — deliberately
-- unlike member rows, which tick per activation. `folded` grows a cheap
-- activity hint on a collapsed row: the working member's short name trails
-- the timer (`● lead (4) 57s →llm`), so the loop's cycling stays visible
-- without expanding. Kept to one member (the first working) — 36 cols.
local function group_row_parts(group, now_ms, folded)
  local glyph = GLYPHS[group.status] or "·"
  local style = NODE_STYLE[group.status] or STYLE.status_dim
  local elapsed
  if group.status == "running" then
    elapsed = now_ms - (group.first_start or now_ms)
  elseif TERMINAL_STATUS[group.status] and group.last_finish then
    elapsed = group.last_finish - (group.first_start or group.last_finish)
  end
  local elapsed_str = elapsed and (" " .. fmt_elapsed_ms(elapsed)) or ""
  local n = #group.members
  local count_str = n > 1 and (" (" .. n .. ")") or ""
  local busy_str = ""
  if folded then
    for _, m in ipairs(group.members) do
      if m.node.status == "working" and m.id ~= group.name then
        busy_str = " →" .. m.id:sub(#group.name + 2)
        break
      end
    end
  end
  local text = glyph .. " " .. group.name .. count_str .. elapsed_str .. busy_str
  return text, style
end

-- Run header. Counts GROUPS (not raw actors): "(done/total)" where a
-- group is done once its aggregated status is terminal, with a trailing
-- rejected / no-op modification tail.
local function mag_run_header_title(run, groups)
  local ident
  if type(run.run_name) == "string" and #run.run_name > 0 then
    ident = run.run_name
  else
    ident = run.run_id and run.run_id:sub(1, 8) or "?"
  end
  local label = "MAG"
  local done = 0
  for _, g in ipairs(groups) do
    if TERMINAL_STATUS[g.status] then done = done + 1 end
  end
  local title = string.format("%s %s (%d/%d)", label, ident, done, #groups)
  local extra = {}
  if (run.rejected or 0) > 0 then extra[#extra + 1] = "✗" .. run.rejected .. " rej" end
  if (run.noops or 0) > 0 then extra[#extra + 1] = "⊘" .. run.noops end
  if #extra > 0 then title = title .. "  " .. table.concat(extra, " ") end
  return title
end

-- MAG member (actor leaf) row, rendered under an unfolded group:
-- `  ● explorer.llm  working 47s` (per-activation timer) or
-- `  · explorer.llm  idle` (between activations — no timer, no noise).
-- The stuck-agent signature — a BUSY actor whose last stream event went
-- stale — comes back as a second return: an indented sub-row
-- (`    ⚠ stale 32s`), following the last_tool sub-row idiom so the alarm
-- survives the narrow sidebar instead of clipping off the row's tail.
-- Idle-between-rounds deliberately never warns: silence is that state's
-- normal shape, only busy-and-silent smells stuck.
local function actor_row_parts(actor_id, node, stream, now_ms)
  local glyph = GLYPHS[node.status] or "·"
  local style = NODE_STYLE[node.status] or STYLE.status_dim
  local elapsed = node_elapsed_ms(node, now_ms)
  local elapsed_str = elapsed and (" " .. fmt_elapsed_ms(elapsed)) or ""
  local text = "  " .. glyph .. " " .. actor_id .. "  "
    .. (node.status or "?") .. elapsed_str
  local stale_text
  if node.status == "working" and stream ~= nil
      and stream.last_activity_ms ~= nil then
    local silent = now_ms - stream.last_activity_ms
    if silent >= agent_streams.STALE_MS then
      stale_text = "    ⚠ stale " .. fmt_elapsed_ms(silent)
    end
  end
  return text, style, stale_text
end

-- ── sidebar row model (fold state / cursor navigation) ────────────────
--
-- The ordered list of navigable sidebar rows. Single source of truth
-- for BOTH rendering order and cursor navigation — update.lua indexes
-- into the same list to move the cursor and resolve Enter, so the
-- highlighted row and the row acted on can never drift apart.
--
-- Per-group stored fold state, DEFAULT COLLAPSED: a group's member
-- actor rows render only while `state.sidebar_folds[run_id][group]` is
-- set (Enter on the group row toggles it — update.lua). Members of a
-- collapsed group are not rows at all, so the cursor skips them by
-- construction. Fold state survives re-renders, focus changes, and run
-- updates; it resets with run prune / a new session (update.lua).
function M.row_model(state, now_ms)
  local rows = {}
  local runs = state.runs or {}
  local streams = state.agent_streams or {}
  local folds = state.sidebar_folds or {}
  for _, run_id in ipairs(sorted_keys(runs)) do
    local run = runs[run_id]
    if not is_expired(run, now_ms) then
      local groups = build_groups(run)
      rows[#rows + 1] = { kind = "run_header", run_id = run_id, run = run, groups = groups }
      local run_streams = streams[run_id] or {}
      local run_folds = folds[run_id] or {}
      for _, g in ipairs(groups) do
        local unfolded = run_folds[g.name] == true
        rows[#rows + 1] = {
          kind = "group", run_id = run_id, group = g, folded = not unfolded,
        }
        if unfolded then
          for _, m in ipairs(g.members) do
            rows[#rows + 1] = {
              kind = "actor", run_id = run_id, actor_id = m.id,
              node = m.node, stream = run_streams[m.id],
            }
          end
        end
      end
    end
  end
  return rows
end

-- Clamp a stored cursor against the current row model (rows shrink when
-- runs prune). Returns 0 only for an empty model.
function M.clamp_cursor(cursor, row_count)
  local cur = cursor or 1
  if cur > row_count then cur = row_count end
  if cur < 1 then cur = math.min(1, row_count) end
  return cur
end

local function panel_children(state, now_ms)
  -- View-side filter note: row_model drops completed runs past their
  -- linger window at paint time so the panel updates on the
  -- wallclock_tick re-render even though the reducer-side `prune` only
  -- runs on a fresh dispatch. Mirrors the toast widget's
  -- defence-in-depth filter at view-time.
  local focused = state.focus == "sidebar"
  local rows = M.row_model(state, now_ms)
  local cursor = M.clamp_cursor(state.sidebar_cursor, #rows)
  local children = {}
  local first_run = true
  for i, row in ipairs(rows) do
    local on_cursor = focused and i == cursor
    if row.kind == "run_header" then
      if not first_run then
        children[#children + 1] = tui.text { content = "", wrap = "none" }
      end
      first_run = false
      local title = mag_run_header_title(row.run, row.groups)
      children[#children + 1] = tui.text {
        content = title,
        style   = on_cursor and CURSOR_ROW_STYLE or STYLE.footer,
        wrap    = "none",
      }
    elseif row.kind == "group" then
      local text, style = group_row_parts(row.group, now_ms, row.folded)
      children[#children + 1] = tui.text {
        content = text,
        style   = on_cursor and CURSOR_ROW_STYLE or style,
        wrap    = "none",
      }
    elseif row.kind == "actor" then
      local text, style, stale_text = actor_row_parts(row.actor_id, row.node, row.stream, now_ms)
      children[#children + 1] = tui.text {
        content = text,
        style   = on_cursor and CURSOR_ROW_STYLE or style,
        wrap    = "none",
      }
      if stale_text ~= nil then
        children[#children + 1] = tui.text {
          content = stale_text,
          style   = STYLE.panel_stale,
          wrap    = "none",
        }
      end
    end
  end
  if #children == 0 then
    children[#children + 1] = tui.text {
      content = "(no active runs)",
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  return children
end

function M.panel(state)
  local now_ms = tui.now_ms()
  local children = {
    tui.text { content = "Graph", style = STYLE.footer, wrap = "none" },
    tui.text { content = string.rep("─", 30), style = STYLE.footer, wrap = "none" },
  }
  for _, c in ipairs(panel_children(state, now_ms)) do
    children[#children + 1] = c
  end
  return tui.constrained {
    min_width = 28,
    max_width = 36,
    child = tui.padding {
      value = 1,
      -- Drag-to-select scopes to this column. The sidebar doesn't
      -- scroll, so the selection's content geometry equals the
      -- column's painted rect — the engine paints into a rect-sized
      -- scratch buffer and extracts plain text. Keyed so the engine
      -- can re-resolve the captured widget across view rebuilds.
      child = tui.column {
        gap        = 0,
        key        = "sidebar",
        selectable = true,
        children   = children,
      },
    },
  }
end

function M.vertical_separator()
  return tui.constrained {
    min_width = 1,
    max_width = 1,
    child = tui.fill { char = "│", style = STYLE.panel_separator },
  }
end

local function apply(state, run_id, fn)
  local prev_runs = state.runs or {}
  local new_runs = {}
  for k, v in pairs(prev_runs) do new_runs[k] = v end
  new_runs[run_id] = fn(prev_runs[run_id])
  return shallow_merge(state, { runs = new_runs })
end

-- User-initiated interrupt (double-ESC). Flip every still-running
-- node to `error` so it renders red — "interrupted" is a failure
-- from the run's POV, same as a backend crash. Stamp completed_at_ms
-- so the linger window starts running and the run fades out via the
-- existing prune path; otherwise the sidebar would freeze with stale
-- "Ns" timers because cancel_all on the engine side never emits the
-- run.completed envelope a clean termination would.
function M.interrupt_all(state, now_ms)
  if type(state.runs) ~= "table" then return state end
  local new_runs = {}
  for run_id, run in pairs(state.runs) do
    local nodes = {}
    for node_id, node in pairs(run.nodes or {}) do
      if not TERMINAL_STATUS[node.status] then
        nodes[node_id] = shallow_merge(node, {
          status         = "error",
          finished_at_ms = now_ms,
        })
      else
        nodes[node_id] = node
      end
    end
    new_runs[run_id] = shallow_merge(run, {
      nodes           = nodes,
      completed_at_ms = run.completed_at_ms or now_ms,
      status          = run.status or "interrupted",
    })
  end
  return shallow_merge(state, { runs = new_runs })
end

-- ── MAG actor-kernel lifecycle ────────────────────────────────────────
--
-- The kernel's `mag.*` event stream drives the run panel. Actors are
-- the panel's "nodes", with activity-honest states: spawned → pending,
-- ready → idle (constructed), busy → working (ticks its current
-- activation), idle → idle (between activations, no timer), killed →
-- killed (distinct), run-complete → the run finalises (still-live
-- actors flip to done). Every event carries its run_id; the reducer
-- keys panel state straight off it.

function M.mag_run_started(state, run_id, run_name, now_ms)
  if state.runs and state.runs[run_id] then return state end
  return apply(state, run_id, function(_)
    return {
      run_id = run_id, run_name = run_name,
      total_nodes = 0, started_at_ms = now_ms, nodes = {},
      completed_at_ms = nil, status = nil, rejected = 0, noops = 0,
      actor_seq = 0,
    }
  end)
end

function M.actor_spawned(state, run_id, actor_id, factory, now_ms)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    if prev.nodes and prev.nodes[actor_id] then return prev end
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local seq = (prev.actor_seq or 0) + 1
    nodes[actor_id] = {
      reasoner = factory or "",
      status = "pending",
      started_at_ms = now_ms,
      finished_at_ms = nil,
      -- First-appearance order for namespace grouping (stable across the
      -- unordered `nodes` map).
      seq = seq,
    }
    return shallow_merge(prev, { nodes = nodes, actor_seq = seq })
  end)
end

-- Shared body for the constructed-and-alive transitions: ready → "idle"
-- (constructed; the kernel's actor_busy follows immediately when work
-- starts), busy → "working" (stamps the CURRENT activation's start so the
-- row's timer resets per activation). An event without a prior spawn
-- observation (out-of-order tail) still surfaces as a node rather than
-- being dropped.
local function mark_actor(state, run_id, actor_id, now_ms, status, extra)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[actor_id] or { reasoner = "", started_at_ms = now_ms }
    local actor_seq = prev.actor_seq or 0
    local seq = node.seq
    if seq == nil then
      actor_seq = actor_seq + 1
      seq = actor_seq
    end
    local patch = {
      status = status,
      started_at_ms = node.started_at_ms or now_ms,
      seq = seq,
    }
    for k, v in pairs(extra or {}) do patch[k] = v end
    nodes[actor_id] = shallow_merge(node, patch)
    return shallow_merge(prev, { nodes = nodes, actor_seq = actor_seq })
  end)
end

function M.actor_ready(state, run_id, actor_id, now_ms)
  return mark_actor(state, run_id, actor_id, now_ms, "idle")
end

-- `mag.actor_busy` — an activation was delivered; the member ticks its
-- current activation from here.
function M.actor_busy(state, run_id, actor_id, now_ms)
  return mark_actor(state, run_id, actor_id, now_ms, "working", {
    activation_started_at_ms = now_ms,
  })
end

-- `mag.actor_idle` — the activation settled; back to quiet idle. Only a
-- working member flips (a killed/done straggler keeps its terminal state;
-- an unseen id has nothing to settle).
function M.actor_idle(state, run_id, actor_id, now_ms)
  if not (state.runs and state.runs[run_id]
      and state.runs[run_id].nodes
      and state.runs[run_id].nodes[actor_id]
      and state.runs[run_id].nodes[actor_id].status == "working") then
    return state
  end
  return mark_actor(state, run_id, actor_id, now_ms, "idle")
end

-- `reason` is the kernel's teardown taxonomy (mag-kernel observer.lua):
-- "run_complete" marks the bookkeeping sweep after a SUCCESSFUL completion —
-- the node stays/goes done (✓), never killed, so a finished run doesn't
-- repaint red. Every other reason ("modification" / "run_failed" / "killed" /
-- "reaped", or absent) renders killed (⊗) as before; the group rule "killed
-- if any member killed" thereby scopes to non-teardown kills.
function M.actor_killed(state, run_id, actor_id, now_ms, reason)
  if not (state.runs and state.runs[run_id]
      and state.runs[run_id].nodes
      and state.runs[run_id].nodes[actor_id]) then
    return state
  end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[actor_id]
    if reason == "run_complete" then
      -- mag.run_complete normally precedes the teardown and already flipped
      -- live nodes to done; flip any straggler here and leave terminal
      -- states (done/killed/error) untouched.
      if TERMINAL_STATUS[node.status] then
        return prev
      end
      nodes[actor_id] = shallow_merge(node, {
        status = "done", finished_at_ms = now_ms,
      })
      return shallow_merge(prev, { nodes = nodes })
    end
    nodes[actor_id] = shallow_merge(node, {
      status = "killed", finished_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

function M.modification_rejected(state, run_id)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    return shallow_merge(prev, { rejected = (prev.rejected or 0) + 1 })
  end)
end

function M.modification_noop(state, run_id)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    return shallow_merge(prev, { noops = (prev.noops or 0) + 1 })
  end)
end

-- Terminal completion for a kernel run. There is no per-actor "done" event
-- in the stream, so every still-live actor flips to done here; a killed
-- actor keeps its terminal state. Stamps `completed_at_ms` so the linger +
-- prune path fades the run out.
function M.mag_run_complete(state, run_id, status, now_ms)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do
      if not TERMINAL_STATUS[v.status] then
        nodes[k] = shallow_merge(v, { status = "done", finished_at_ms = now_ms })
      else
        nodes[k] = v
      end
    end
    return shallow_merge(prev, {
      nodes = nodes, completed_at_ms = now_ms, status = status or "success",
    })
  end)
end

-- ── fold state (per-run, per-group; default collapsed) ────────────────
--
-- `state.sidebar_folds[run_id][group] = true` marks a group UNFOLDED —
-- absence is the collapsed default, so a fresh run renders collapsed and
-- pruning a run's entry restores the default for a reused run_id.

function M.toggle_fold(state, run_id, group_name)
  local prev = state.sidebar_folds or {}
  local prev_run = prev[run_id] or {}
  local next_run = {}
  for k, v in pairs(prev_run) do next_run[k] = v end
  next_run[group_name] = (not prev_run[group_name]) or nil
  local next_folds = {}
  for k, v in pairs(prev) do next_folds[k] = v end
  next_folds[run_id] = next_run
  return shallow_merge(state, { sidebar_folds = next_folds })
end

-- Drop fold entries for runs no longer live (the pruned run map), so fold
-- state follows the run lifecycle exactly like the capture buffers do
-- (agent_streams.prune).
function M.prune_folds(state, runs)
  local folds = state.sidebar_folds
  if type(folds) ~= "table" then return state end
  local live = runs or {}
  local stale = false
  for run_id in pairs(folds) do
    if live[run_id] == nil then stale = true break end
  end
  if not stale then return state end
  local kept = {}
  for run_id, v in pairs(folds) do
    if live[run_id] ~= nil then kept[run_id] = v end
  end
  return shallow_merge(state, { sidebar_folds = kept })
end

return M
