-- Run-panel sidebar widget: renders the active kernel runs as a column
-- of run-headers + grouped actor rows, and owns the pure-state mutators
-- (mag_run_started, actor_* transitions, mag_run_complete, prune) the
-- chat reducer calls.

local common = require("libs.chat.common")
local preview_state = require("libs.chat.preview_state")
local STYLE   = common.STYLE
local CURSOR_ROW_STYLE = common.CURSOR_ROW_STYLE
local shallow_merge = common.shallow_merge
local NIL_SENTINEL = common.NIL_SENTINEL

local M = {}

M.LINGER_MS = 2000

local GLYPHS = {
  pending = "○",
  running = "●",
  done    = "✓",
  error   = "✗",
  skipped = "⊘",
  -- A killed actor is a deliberate termination, distinct from a failed
  -- one. Both share the ⊗ glyph — the distinction rides the row's label
  -- text ("killed" vs "failed"), not the shape — so a failed run reads red
  -- and terminated without inventing a second glyph.
  killed  = "⊗",
  failed  = "⊗",
  -- MAG member activity states (mag.actor_busy / mag.actor_idle): `working`
  -- is a live activation (ticks its current activation's elapsed), `idle` is
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
  failed  = STYLE.panel_error,
  working = STYLE.panel_running,
  idle    = STYLE.status_dim,
}

-- Node statuses that count as terminal (drive the done/total counter and
-- freeze the elapsed timer).
local TERMINAL_STATUS = {
  done = true, error = true, skipped = true, killed = true, failed = true,
}

-- Kill reasons (mag.actor_killed `reason`) that mean the RUN itself ended —
-- a control-plane teardown — as opposed to "modification", a mid-run kill
-- that leaves the run live. A teardown reason stamps the run terminal so its
-- linger→prune cycle starts; "run_complete" is excluded here because
-- mag.run_complete already owns that run's completion bookkeeping.
local RUN_TEARDOWN_REASON = {
  run_failed = true, killed = true, reaped = true,
}

local function sorted_keys(m)
  local out = {}
  for k in pairs(m) do out[#out + 1] = k end
  table.sort(out)
  return out
end

local function fmt_elapsed_ms(ms)
  local seconds = math.max(0, math.floor((ms or 0) / 1000))
  local value, unit
  if seconds < 100 then
    value, unit = seconds, "s"
  elseif seconds < 100 * 60 then
    value, unit = math.floor(seconds / 60), "m"
  elseif seconds < 100 * 60 * 60 then
    value, unit = math.floor(seconds / 3600), "h"
  else
    value, unit = math.min(99, math.floor(seconds / 86400)), "d"
  end
  return string.format("%02d%s", value, unit)
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

-- Completed runs leave the active row model after the linger window, but the
-- newest expired run remains structurally retained as a bounded inspection
-- target. A newer completion replaces it; session reset clears the run map.
-- This keeps the main panel quiet without making the just-finished preview
-- unreachable or accumulating an unbounded debug history.
local function newest_expired_id(runs, now_ms)
  local newest_id, newest_at
  for run_id, run in pairs(runs or {}) do
    if is_expired(run, now_ms) and (newest_at == nil
        or run.completed_at_ms > newest_at
        or (run.completed_at_ms == newest_at and run_id > newest_id)) then
      newest_id, newest_at = run_id, run.completed_at_ms
    end
  end
  return newest_id
end

function M.recent_completed(runs, now_ms)
  local run_id = newest_expired_id(runs, now_ms)
  return run_id, run_id and runs[run_id] or nil
end

function M.prune(runs, now_ms)
  if runs == nil then return {} end
  local retained = newest_expired_id(runs, now_ms)
  local pruned = nil
  for run_id, run in pairs(runs) do
    if is_expired(run, now_ms) and run_id ~= retained then
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

-- Elapsed window for an actor row: live-ticking while working (the current
-- activation only) and frozen across terminal state. Idle carries no window — a
-- between-activations actor shows no timer.
local function node_elapsed_ms(node, now_ms)
  if node.status == "running" or node.status == "working" then
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
-- Exported so the composite agent view (popups.lua) can select a group's
-- members by the same namespace rule the panel groups by.
M.group_of = group_of

local function active_groups(nodes)
  local out = {}
  for id, node in pairs(nodes or {}) do
    if node.status == "running" or node.status == "working" then
      out[group_of(id)] = true
    end
  end
  return out
end

-- Track the union of yellow member intervals for each collapsed logical node.
-- Summing actor durations would double-count overlapping work; a first→last
-- window would count idle gaps. This state follows exactly what the group row
-- paints as active.
local function advance_group_activity(prev, nodes, now_ms)
  local current = prev.group_activity or {}
  local next_activity, groups = {}, active_groups(nodes)
  for name, value in pairs(current) do
    next_activity[name] = { active_ms = value.active_ms or 0,
      active_since_ms = value.active_since_ms }
    groups[name] = groups[name] or false
  end
  for name, is_active in pairs(groups) do
    local item = next_activity[name] or { active_ms = 0 }
    if is_active and item.active_since_ms == nil then
      item.active_since_ms = now_ms
    elseif not is_active and item.active_since_ms ~= nil then
      item.active_ms = item.active_ms + now_ms - item.active_since_ms
      item.active_since_ms = nil
    end
    next_activity[name] = item
  end
  return next_activity
end

-- Aggregate actor activity into workflow-node progress. The kernel also calls
-- a newly constructed (but never fired) actor `idle`; `settled_at_ms`
-- distinguishes that ready state from an activation that actually completed.
-- An actor remains resident after settling, while a later busy event moves its
-- workflow node back to running. Actor teardown is tracked separately.
local LIVE_MEMBER_STATUS = { running = true, working = true }

local function group_status(members, run_completed)
  local any_killed, any_failed, any_running, any_pending =
    false, false, false, false
  local count = 0
  for _, m in ipairs(members) do
    count = count + 1
    if m.node.status == "killed" then any_killed = true
    elseif m.node.status == "failed" or m.node.status == "error" then
      any_failed = true
    elseif LIVE_MEMBER_STATUS[m.node.status] then
      any_running = true
    elseif m.node.status == "pending"
        or (m.node.status == "idle" and m.node.started_at_ms == nil) then
      any_pending = true
    end
  end
  if count == 0 then return "pending" end
  if any_killed then return "killed" end
  if any_failed then return "failed" end
  if any_running then return "running" end
  if any_pending and not run_completed then return "pending" end
  return "done"
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
    local activity = (run.group_activity or {})[b.name] or {}
    b.active_ms = activity.active_ms or 0
    b.active_since_ms = activity.active_since_ms
    local first_start, last_finish
    for _, m in ipairs(b.members) do
      local s = m.node.started_at_ms
      if s and (first_start == nil or s < first_start) then first_start = s end
      -- A resident actor is finalized with the whole MAG run, often long
      -- after its own last activation. Prefer that activation boundary so a
      -- completed workflow node does not keep accumulating downstream time.
      local f = m.node.settled_at_ms or m.node.finished_at_ms
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
-- Exported for the composite view's header (member count / aggregated
-- status / activity window of a whole group or run).
M.build_groups = build_groups

local function text(content, style, wrap)
  return tui.text { content = content, style = style, wrap = wrap or "none" }
end

local function duration_widget(elapsed, style)
  return text(fmt_elapsed_ms(elapsed), style)
end

local function name_widget(name, style)
  return tui.expanded { fit = "loose", child = text(name, style, "ellipsis") }
end

local function group_elapsed_ms(group, now_ms)
  if group.status == "running" then
    return group.active_ms + math.max(0, now_ms - (group.active_since_ms or now_ms))
  end
  if TERMINAL_STATUS[group.status] and group.first_start then return group.active_ms end
  return nil
end

local function group_row_widget(group, now_ms, selected)
  local style = selected and CURSOR_ROW_STYLE or NODE_STYLE[group.status] or STYLE.status_dim
  local n = #group.members
  local count = n > 1 and (" (" .. n .. ")") or ""
  return tui.row { gap = 0, children = {
    text((GLYPHS[group.status] or "·") .. " ", style),
    duration_widget(group_elapsed_ms(group, now_ms), style),
    text(" ", style),
    name_widget(group.name, style),
    text(count, style),
  } }
end

local function run_ident(run)
  if type(run.run_name) == "string" and #run.run_name > 0 then return run.run_name end
  return run.run_id and run.run_id:sub(1, 8) or "?"
end

local function run_elapsed_ms(run, now_ms)
  local finish = run.completed_at_ms or now_ms
  return math.max(0, finish - (run.started_at_ms or finish))
end

-- Counts GROUPS (not raw actors). Duration is total workflow wall time,
-- independent of the activity-only timers on group and actor rows.
local function run_header_widget(run, groups, now_ms, selected)
  local style = selected and CURSOR_ROW_STYLE or STYLE.footer
  local done = 0
  for _, group in ipairs(groups) do
    if TERMINAL_STATUS[group.status] then done = done + 1 end
  end
  local extra = {}
  if (run.rejected or 0) > 0 then extra[#extra + 1] = " ✗" .. run.rejected .. " rej" end
  if (run.noops or 0) > 0 then extra[#extra + 1] = " ⊘" .. run.noops end
  return tui.row { gap = 0, children = {
    text("MAG ", style),
    duration_widget(run_elapsed_ms(run, now_ms), style),
    text(" ", style),
    name_widget(run_ident(run), style),
    text(string.format(" (%d/%d)", done, #groups), style),
    text(table.concat(extra), style),
  } }
end

-- Member timers cover the current/cumulative activation. Pending and idle
-- members reserve the same quiet duration slot so sibling names stay aligned.
local function member_label(parent_id, actor_id)
  local prefix = parent_id .. "."
  if actor_id:sub(1, #prefix) == prefix then return actor_id:sub(#prefix + 1) end
  return actor_id
end
M.member_label = member_label

local function actor_row_widget(parent_id, actor_id, node, now_ms, selected)
  local style = selected and CURSOR_ROW_STYLE or NODE_STYLE[node.status] or STYLE.status_dim
  return tui.row { gap = 0, children = {
    text("  " .. (GLYPHS[node.status] or "·") .. " ", style),
    duration_widget(node_elapsed_ms(node, now_ms), style),
    text(" ", style),
    name_widget(member_label(parent_id, actor_id), style),
  } }
end

local function actor_stale_text(node, stream, now_ms)
  if node.status == "working" and stream ~= nil and stream.last_activity_ms ~= nil then
    local silent = now_ms - stream.last_activity_ms
    if silent >= preview_state.STALE_MS then
      return "    ⚠ stale " .. fmt_elapsed_ms(silent)
    end
  end
  return nil
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
-- Runs render in creation order (started_at_ms, run_id tiebreak), not id
-- order: the turn's main run stays first and ad-hoc eval runs append below
-- instead of alphabetically reshuffling the panel on every one-off.
local function runs_in_creation_order(runs)
  local out = {}
  for run_id in pairs(runs) do out[#out + 1] = run_id end
  table.sort(out, function(a, b)
    local ta = runs[a].started_at_ms or 0
    local tb = runs[b].started_at_ms or 0
    if ta ~= tb then return ta < tb end
    return a < b
  end)
  return out
end

function M.row_model(state, now_ms)
  local rows = {}
  local runs = state.runs or {}
  local streams = state.node_previews or {}
  local folds = state.sidebar_folds or {}
  -- Content coordinates inside the scrollable. Padding contributes the first
  -- row, then the two fixed title rows. Every navigable row owns its complete
  -- painted range; callers never reconstruct layout from cursor indices.
  local visual_y = 3
  local first_run = true
  local fixture_mode = os.getenv("NEFOR_TEST_SIDEBAR_OVERFLOW") == "1"
  local function append(row, height)
    row.visual_start = visual_y
    row.visual_end = visual_y + height
    visual_y = row.visual_end
    rows[#rows + 1] = row
  end
  for _, run_id in ipairs(runs_in_creation_order(runs)) do
    local run = runs[run_id]
    if fixture_mode or not is_expired(run, now_ms) then
      if not first_run then visual_y = visual_y + 1 end
      first_run = false
      local groups = build_groups(run)
      append({ kind = "run_header", run_id = run_id, run = run, groups = groups }, 1)
      local run_streams = streams[run_id] or {}
      local run_folds = folds[run_id] or {}
      for _, g in ipairs(groups) do
        local unfolded = run_folds[g.name] == true
        append({ kind = "group", run_id = run_id, group = g }, 1)
        if unfolded then
          for _, m in ipairs(g.members) do
            local stream = run_streams[m.id]
            local stale_text = actor_stale_text(m.node, stream, now_ms)
            append({
              kind = "actor", run_id = run_id, parent_id = g.name, actor_id = m.id,
              node = m.node, stream = stream,
            }, stale_text ~= nil and 2 or 1)
          end
        end
      end
    end
  end
  return rows
end

function M.selected_visual_range(state, now_ms)
  local rows = M.row_model(state, now_ms)
  local cursor = M.clamp_cursor(state.sidebar_cursor, #rows)
  local row = rows[cursor]
  if row == nil then return nil, nil, cursor, rows end
  return row.visual_start, row.visual_end, cursor, rows
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
      children[#children + 1] = run_header_widget(row.run, row.groups, now_ms, on_cursor)
    elseif row.kind == "group" then
      children[#children + 1] = group_row_widget(row.group, now_ms, on_cursor)
    elseif row.kind == "actor" then
      local stale_text = actor_stale_text(row.node, row.stream, now_ms)
      children[#children + 1] = actor_row_widget(
        row.parent_id, row.actor_id, row.node, now_ms, on_cursor)
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
    local recent_id = M.recent_completed(state.runs, now_ms)
    children[#children + 1] = tui.text {
      content = recent_id and "Space: inspect last completed run" or "(no active runs)",
      style   = STYLE.status_dim,
      wrap    = "word",
    }
  end
  return children
end

function M.panel(state)
  local now_ms = tui.now_ms()
  -- Focused pane treatment: the title bar switches to the bright/bold
  -- highlight vocabulary (popup_user) and carries an explicit focus
  -- marker, so an active sidebar is unmistakable at a glance. Unfocused
  -- it reads as quiet chrome (footer), exactly as before. The cursor row
  -- stays THE in-pane focus indicator (CURSOR_ROW_STYLE, focus-gated in
  -- panel_children).
  local focused = state.focus == "sidebar"
  local title_style = focused and STYLE.popup_user or STYLE.footer
  local title_text  = focused and "▌ Workflows · focused" or "Workflows"
  local children = {
    tui.text { content = title_text, style = title_style, wrap = "none" },
    tui.text { content = string.rep("─", 30), style = title_style, wrap = "none" },
  }
  for _, c in ipairs(panel_children(state, now_ms)) do
    children[#children + 1] = c
  end
  return tui.constrained {
    min_width = 28,
    max_width = 36,
    child = tui.scrollable {
      key = "sidebar",
      stick_to = "start",
      scrollbar = "auto",
      selectable = true,
      child = tui.padding {
        value = 1,
        child = tui.column { gap = 0, children = children },
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

-- ── MAG actor-kernel lifecycle ────────────────────────────────────────
--
-- The kernel's `mag.*` event stream drives the run panel. Actors are
-- the panel's "nodes", with activity-honest states: spawned → pending,
-- ready → idle (constructed), busy → working (ticks its current
-- activation), idle → idle (between activations, no timer), killed →
-- killed (distinct), run-complete → the run finalises (still-live
-- actors flip to done). Every event carries its run_id; the reducer
-- keys panel state straight off it.

function M.mag_run_started(state, run_id, run_name, principal, now_ms)
  if state.runs and state.runs[run_id] then return state end
  return apply(state, run_id, function(_)
    return {
      run_id = run_id, run_name = run_name, principal = principal,
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
      spawned_at_ms = now_ms,
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
-- starts), busy → "working" (stamps the current activation's start). An event without a prior spawn
-- observation (out-of-order tail) still surfaces as a node rather than
-- being dropped.
local function mark_actor(state, run_id, actor_id, now_ms, status, extra)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[actor_id] or { reasoner = "", spawned_at_ms = now_ms }
    local actor_seq = prev.actor_seq or 0
    local seq = node.seq
    if seq == nil then
      actor_seq = actor_seq + 1
      seq = actor_seq
    end
    local patch = {
      status = status,
      seq = seq,
    }
    for k, v in pairs(extra or {}) do patch[k] = v end
    nodes[actor_id] = shallow_merge(node, patch)
    return shallow_merge(prev, { nodes = nodes, actor_seq = actor_seq,
      group_activity = advance_group_activity(prev, nodes, now_ms) })
  end)
end

function M.actor_ready(state, run_id, actor_id, now_ms)
  return mark_actor(state, run_id, actor_id, now_ms, "idle")
end

-- `mag.actor_busy` — an activation was delivered; the member ticks its
-- current activation from here while retaining prior active intervals.
function M.actor_busy(state, run_id, actor_id, now_ms)
  local node = ((state.runs or {})[run_id].nodes or {})[actor_id] or {}
  return mark_actor(state, run_id, actor_id, now_ms, "working", {
    started_at_ms = node.started_at_ms or now_ms,
    activation_started_at_ms = node.status == "working"
      and node.activation_started_at_ms or now_ms,
    settled_at_ms = NIL_SENTINEL,
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
  return mark_actor(state, run_id, actor_id, now_ms, "idle", {
    settled_at_ms = now_ms,
  })
end

-- `reason` is the kernel's teardown taxonomy (mag-kernel observer.lua) and
-- drives BOTH the node label and the run's terminal bookkeeping:
--   * "run_complete" — the sweep after a SUCCESSFUL completion; the node
--     stays/goes done (✓), never killed, so a finished run doesn't repaint
--     red. mag.run_complete owns the run's completed_at_ms stamp.
--   * "run_failed" — a run-failure teardown; the node reads "failed" (⊗,
--     red) and the RUN is stamped terminal here so its linger→prune starts
--     even when no mag.run_failed reached the panel first.
--   * "killed" / "reaped" — an outright kill_run / session-sweep teardown;
--     the node reads "killed" (⊗) and the RUN is stamped terminal (kill_run
--     emits no run-level event, so this is the only prune trigger for it).
--   * "modification" (or absent) — a mid-run control-plane kill; the node
--     reads "killed" but the run stays LIVE (not stamped) — it continues.
-- An already-terminal node is never downgraded, so a stray or second
-- teardown of a settled run can't repaint done→killed.
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
      -- states (done/killed/failed/error) untouched.
      if not TERMINAL_STATUS[node.status] then
        nodes[actor_id] = shallow_merge(node, {
          status = "done", finished_at_ms = now_ms,
        })
      end
      return shallow_merge(prev, { nodes = nodes,
        group_activity = advance_group_activity(prev, nodes, now_ms) })
    end
    local new_status = (reason == "run_failed") and "failed" or "killed"
    if not TERMINAL_STATUS[node.status] then
      nodes[actor_id] = shallow_merge(node, {
        status = new_status, finished_at_ms = now_ms,
      })
    end
    local patch = { nodes = nodes,
      group_activity = advance_group_activity(prev, nodes, now_ms) }
    if RUN_TEARDOWN_REASON[reason] then
      patch.completed_at_ms = prev.completed_at_ms or now_ms
      patch.status = prev.status or new_status
    end
    return shallow_merge(prev, patch)
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
        nodes[k] = shallow_merge(v, {
          status = "done", finished_at_ms = now_ms,
          settled_at_ms = v.settled_at_ms
            or ((v.status == "working" or v.status == "running") and now_ms or nil),
        })
      else
        nodes[k] = v
      end
    end
    return shallow_merge(prev, {
      nodes = nodes, completed_at_ms = now_ms, status = status or "success",
      group_activity = advance_group_activity(prev, nodes, now_ms),
    })
  end)
end

-- Terminal FAILURE for a kernel run (mag.run_failed): the run ended on an
-- unhandled actor failure. Mirrors mag_run_complete's linger bookkeeping —
-- stamps `completed_at_ms` so the same linger→prune path fades the run out —
-- but flips still-live actors to "failed" (not "done"), so a failed run
-- reads red. The teardown's actor_killed(reason="run_failed") events land on
-- the now-terminal nodes afterwards and leave them (actor_killed's terminal
-- guard). Idempotent with that path: whichever arrives first stamps the run.
function M.mag_run_failed(state, run_id, status, now_ms)
  if not (state.runs and state.runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do
      if not TERMINAL_STATUS[v.status] then
        nodes[k] = shallow_merge(v, {
          status = "failed", finished_at_ms = now_ms,
          settled_at_ms = v.settled_at_ms
            or ((v.status == "working" or v.status == "running") and now_ms or nil),
        })
      else
        nodes[k] = v
      end
    end
    return shallow_merge(prev, {
      nodes = nodes, completed_at_ms = prev.completed_at_ms or now_ms,
      status = status or "failed",
      group_activity = advance_group_activity(prev, nodes, now_ms),
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
-- (preview_state.prune).
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
