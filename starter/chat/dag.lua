-- DAG sidebar widget: renders the active reasoner-graph runs as a
-- column of run-headers + per-node rows, and owns the small set of
-- pure-state mutators (run_started, node_dispatched, node_result,
-- run_complete, prune) the chat reducer calls.

local common = require("chat.common")
local STYLE   = common.STYLE
local shallow_merge = common.shallow_merge

local M = {}

M.LINGER_MS = 2000

local GLYPHS = {
  pending = "○",
  running = "●",
  done    = "✓",
  error   = "✗",
  skipped = "⊘",
  -- MAG-only: a killed actor is a deliberate termination, distinct from a
  -- failed one (reasoner-graph never had this state — richer-than-parity).
  killed  = "⊗",
}

local NODE_STYLE = {
  pending = STYLE.dag_pending,
  running = STYLE.dag_running,
  done    = STYLE.dag_done,
  error   = STYLE.dag_error,
  skipped = STYLE.dag_skipped,
  killed  = STYLE.dag_error,
}

-- Node statuses that count as terminal (drive the done/total counter and
-- freeze the elapsed timer). MAG's `killed` joins the reasoner-graph set.
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

-- A completed run lingers in the panel for `LINGER_MS` after its
-- `completed_at_ms` so the user can see the final state. Past that
-- window the run is dropped — visually by the view (this helper),
-- structurally by `prune` on the next reducer dispatch.
local function is_expired(run, now_ms)
  return run.completed_at_ms ~= nil
     and (now_ms - run.completed_at_ms) > M.LINGER_MS
end

function M.prune(dag_runs, now_ms)
  if dag_runs == nil then return {} end
  local pruned = nil
  for run_id, run in pairs(dag_runs) do
    if is_expired(run, now_ms) then
      if pruned == nil then
        pruned = {}
        for k, v in pairs(dag_runs) do pruned[k] = v end
      end
      pruned[run_id] = nil
    end
  end
  return pruned or dag_runs
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
function M.any_active(dag_runs, now_ms)
  if type(dag_runs) ~= "table" then return false end
  for _, run in pairs(dag_runs) do
    if not is_expired(run, now_ms or 0) then return true end
  end
  return false
end

local function run_header(run)
  -- Reasoner-graph runs key on the abbreviated run id; MAG runs carry a
  -- human `run_name` (the .mag graph name) worth showing verbatim.
  local ident
  if type(run.run_name) == "string" and #run.run_name > 0 then
    ident = run.run_name
  else
    ident = run.run_id and run.run_id:sub(1, 8) or "?"
  end
  local label = run.label or "DAG"
  local total = run.total_nodes or 0
  local nodes = run.nodes or {}
  local done = 0
  local nodes_count = 0
  for _, n in pairs(nodes) do
    nodes_count = nodes_count + 1
    if TERMINAL_STATUS[n.status] then
      done = done + 1
    end
  end
  if nodes_count > total then total = nodes_count end
  local title = string.format("%s %s (%d/%d)", label, ident, done, total)
  -- Rejected / no-op modification counters (MAG-only) trail the counter so
  -- a run that is churning against validation reads distinctly.
  local extra = {}
  if (run.rejected or 0) > 0 then extra[#extra + 1] = "✗" .. run.rejected .. " rej" end
  if (run.noops or 0) > 0 then extra[#extra + 1] = "⊘" .. run.noops end
  if #extra > 0 then title = title .. "  " .. table.concat(extra, " ") end
  return tui.text { content = title, style = STYLE.footer, wrap = "none" }
end

local function node_rows(node_id, node, now_ms, narrow)
  local glyph = GLYPHS[node.status] or "·"
  local style = NODE_STYLE[node.status] or STYLE.status_dim
  local elapsed
  if node.status == "running" then
    elapsed = now_ms - (node.started_at_ms or now_ms)
  elseif node.status == "done" or node.status == "error" or node.status == "killed" then
    if node.finished_at_ms ~= nil then
      elapsed = node.finished_at_ms - (node.started_at_ms or node.finished_at_ms)
    end
  end
  local elapsed_str = elapsed and (" " .. fmt_elapsed_ms(elapsed)) or ""
  local text
  if narrow then
    text = glyph .. " " .. node_id .. elapsed_str
  else
    local reasoner = node.reasoner or ""
    local status_word = node.status or "?"
    text = string.format("%s %s  %s  %s%s",
      glyph, node_id, reasoner, status_word, elapsed_str)
  end
  local rows = { tui.text { content = text, style = style, wrap = "none" } }
  -- Indented sub-line: "what the agent inside this node is doing
  -- right now" (last tool dispatched to tool-gate). Only shown while
  -- the node is running — once it terminates, the status glyph + the
  -- transcript carry the signal and the leftover tool name is noise.
  if node.status == "running" and type(node.last_tool) == "string"
      and #node.last_tool > 0 then
    local label = node.last_tool
    if type(node.last_tool_args) == "string" and #node.last_tool_args > 0 then
      label = label .. "(" .. node.last_tool_args .. ")"
    end
    rows[#rows + 1] = tui.text {
      content = "  → " .. label,
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  return rows
end

-- ── MAG namespace grouping (display model) ────────────────────────────
--
-- The MAG run panel collapses a run's actors into top-level namespace
-- groups: an actor id's group is its segment up to the first ".". Group
-- rows aggregate member state; per-member states are retained inside each
-- group (a future expand/collapse toggle reads them straight off the same
-- `run.nodes` store). An undotted id (e.g. `sink`) is its own single-
-- member group. This path is MAG-only — reasoner-graph runs keep the flat
-- per-node rendering in `run_header` / `node_rows`.

local function group_of(actor_id)
  return actor_id:match("^([^.]+)") or actor_id
end

-- Aggregate a group's member states into one status. Precedence, highest
-- first: killed (any member killed) → running (any member live) → done
-- (run finished) → pending (no member ready yet). `killed` outranks
-- `running` so a group with any killed member reads as terminated — the
-- "killed if any member was killed" rule — even while a sibling is still
-- mid-flight.
local function group_status(members, run_completed)
  local any_killed, any_running = false, false
  local count = 0
  for _, m in ipairs(members) do
    count = count + 1
    if m.node.status == "killed" then any_killed = true
    elseif m.node.status == "running" then any_running = true end
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

local function group_row(group, now_ms)
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
  local text = glyph .. " " .. group.name .. count_str .. elapsed_str
  return tui.text { content = text, style = style, wrap = "none" }
end

-- MAG run header. Counts GROUPS (not raw actors): "(done/total)" where a
-- group is done once its aggregated status is terminal. Mirrors the flat
-- header's rejected / no-op modification tail.
local function mag_run_header(run, groups)
  local ident
  if type(run.run_name) == "string" and #run.run_name > 0 then
    ident = run.run_name
  else
    ident = run.run_id and run.run_id:sub(1, 8) or "?"
  end
  local label = run.label or "MAG"
  local done = 0
  for _, g in ipairs(groups) do
    if TERMINAL_STATUS[g.status] then done = done + 1 end
  end
  local title = string.format("%s %s (%d/%d)", label, ident, done, #groups)
  local extra = {}
  if (run.rejected or 0) > 0 then extra[#extra + 1] = "✗" .. run.rejected .. " rej" end
  if (run.noops or 0) > 0 then extra[#extra + 1] = "⊘" .. run.noops end
  if #extra > 0 then title = title .. "  " .. table.concat(extra, " ") end
  return tui.text { content = title, style = STYLE.footer, wrap = "none" }
end

local function panel_children(state, now_ms, narrow)
  local children = {}
  local run_ids = sorted_keys(state.dag_runs)
  local first = true
  for _, run_id in ipairs(run_ids) do
    local run = state.dag_runs[run_id]
    -- View-side filter: a completed run past its linger window is
    -- dropped at paint time so the panel updates on the
    -- wallclock_tick re-render even though the reducer-side `prune`
    -- only runs on a fresh dispatch. Without this, the completed
    -- run stayed visible (all nodes green) until the next user
    -- keystroke flushed prune through the reducer. Mirrors the
    -- toast widget's defence-in-depth filter at view-time.
    if not is_expired(run, now_ms) then
      if not first then
        children[#children + 1] = tui.text { content = "", wrap = "none" }
      end
      first = false
      if run.label == "MAG" then
        -- MAG panel path: group actors by namespace, one row per group.
        local groups = build_groups(run)
        children[#children + 1] = mag_run_header(run, groups)
        for _, g in ipairs(groups) do
          children[#children + 1] = group_row(g, now_ms)
        end
      else
        children[#children + 1] = run_header(run)
        local node_ids = sorted_keys(run.nodes or {})
        for _, node_id in ipairs(node_ids) do
          for _, row in ipairs(node_rows(node_id, run.nodes[node_id], now_ms, narrow)) do
            children[#children + 1] = row
          end
        end
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
  local narrow = true
  local now_ms = tui.now_ms()
  local children = {
    tui.text { content = "Graph", style = STYLE.footer, wrap = "none" },
    tui.text { content = string.rep("─", 30), style = STYLE.footer, wrap = "none" },
  }
  for _, c in ipairs(panel_children(state, now_ms, narrow)) do
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
    child = tui.fill { char = "│", style = STYLE.dag_separator },
  }
end

local function apply(state, run_id, fn)
  local prev_runs = state.dag_runs or {}
  local new_runs = {}
  for k, v in pairs(prev_runs) do new_runs[k] = v end
  new_runs[run_id] = fn(prev_runs[run_id])
  return shallow_merge(state, { dag_runs = new_runs })
end

function M.run_started(state, run_id, total_nodes, now_ms)
  if state.dag_runs and state.dag_runs[run_id] then return state end
  return apply(state, run_id, function(_)
    return {
      run_id = run_id, total_nodes = total_nodes or 0,
      started_at_ms = now_ms, nodes = {},
      completed_at_ms = nil, status = nil,
    }
  end)
end

function M.node_dispatched(state, run_id, node_id, reasoner, now_ms)
  return apply(state, run_id, function(prev)
    local run = prev or {
      run_id = run_id, total_nodes = 0, started_at_ms = now_ms,
      nodes = {}, completed_at_ms = nil,
    }
    local nodes = {}
    for k, v in pairs(run.nodes or {}) do nodes[k] = v end
    nodes[node_id] = {
      reasoner = reasoner or "",
      status = "running",
      started_at_ms = now_ms,
      finished_at_ms = nil,
    }
    return shallow_merge(run, { nodes = nodes })
  end)
end

-- Format a tool's args into a short single-line string for the DAG
-- sidebar's "currently calling X" sub-row. The goal is to make
-- parallel agents distinguishable when they happen to use the same
-- tool name (e.g. three explorers all running `bash` with different
-- commands). Per-tool extractors for the common cases; generic first-
-- string-arg fallback for everything else.
local TOOL_ARG_KEYS = {
  bash         = { "command" },
  read_file    = { "path", "file_path" },
  read_image   = { "path", "file_path" },
  write_file   = { "path", "file_path" },
  edit_file    = { "path", "file_path", "target_path" },
  list_dir     = { "path" },
  search_text  = { "pattern", "query", "text" },
}

local PATH_TOOLS = {
  read_file  = true,
  read_image = true,
  write_file = true,
  edit_file  = true,
  list_dir   = true,
}

local function format_path_arg_short(path, max_len)
  if #path <= max_len then return path end
  local prefix = ".../"
  local budget = max_len - #prefix
  local parts = {}
  for part in path:gmatch("[^/]+") do parts[#parts + 1] = part end
  local tail = ""
  for i = #parts, 1, -1 do
    local candidate = tail == "" and parts[i] or (parts[i] .. "/" .. tail)
    if #candidate > budget then break end
    tail = candidate
  end
  if tail == "" then tail = path:sub(-budget) end
  return prefix .. tail
end

local function format_tool_args_short(tool_name, args)
  if type(args) ~= "table" then return "" end
  local keys = TOOL_ARG_KEYS[tool_name]
  local picked
  if keys then
    for _, k in ipairs(keys) do
      local v = args[k]
      if type(v) == "string" and #v > 0 then picked = v; break end
    end
  end
  if picked == nil then
    -- Generic: first string-valued arg (sorted-key order for
    -- determinism so the same args render the same way each turn).
    local sorted = {}
    for k, _ in pairs(args) do
      if type(k) == "string" then sorted[#sorted + 1] = k end
    end
    table.sort(sorted)
    for _, k in ipairs(sorted) do
      local v = args[k]
      if type(v) == "string" and #v > 0 then picked = v; break end
    end
  end
  if picked == nil then return "" end
  -- Compact whitespace + truncate. Newlines turn the row multi-line
  -- and break sidebar layout; replace them.
  picked = picked:gsub("[\r\n]+", " ")
  local MAX = 40
  if PATH_TOOLS[tool_name] then
    picked = format_path_arg_short(picked, MAX)
  elseif #picked > MAX then
    picked = picked:sub(1, MAX - 1) .. "…"
  end
  return picked
end

function M.node_tool_invoked(state, run_id, node_id, tool_name, tool_args, now_ms)
  -- Only stamp progress for nodes we've observed dispatch for. If we
  -- haven't seen `graph.node.fired` for this (run, node) yet — out-of-
  -- order delivery, replay tail, whatever — drop quietly rather than
  -- synthesise a partial node row that misses `reasoner` / start time.
  if not (state.dag_runs and state.dag_runs[run_id]
      and state.dag_runs[run_id].nodes
      and state.dag_runs[run_id].nodes[node_id]) then
    return state
  end
  local short_args = format_tool_args_short(tool_name, tool_args)
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    nodes[node_id] = shallow_merge(nodes[node_id], {
      last_tool       = tool_name,
      last_tool_args  = short_args,
      last_tool_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

function M.node_result(state, run_id, node_id, has_output, has_error, now_ms)
  local terminal_status
  if has_output then terminal_status = "done"
  elseif has_error then terminal_status = "error"
  else terminal_status = "error" end
  -- Drop results for nodes we haven't observed dispatch for. In live
  -- mode this shouldn't happen; if it does, the result is visible in
  -- logs and that's the right place to investigate, not a synthetic
  -- panel entry that papers over the gap.
  if not (state.dag_runs and state.dag_runs[run_id]
      and state.dag_runs[run_id].nodes
      and state.dag_runs[run_id].nodes[node_id]) then
    return state
  end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[node_id]
    nodes[node_id] = shallow_merge(node, {
      status = terminal_status, finished_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

-- User-initiated interrupt (double-ESC). Flip every still-running
-- node to `error` so it renders red — "interrupted" is a failure
-- from the run's POV, same as a backend crash. Stamp completed_at_ms
-- so the linger window starts running and the run fades out via the
-- existing prune path; otherwise the sidebar would freeze with stale
-- "Ns" timers because cancel_all on the engine side never emits the
-- run.completed envelope a clean termination would.
function M.interrupt_all(state, now_ms)
  if type(state.dag_runs) ~= "table" then return state end
  local new_runs = {}
  for run_id, run in pairs(state.dag_runs) do
    local nodes = {}
    for node_id, node in pairs(run.nodes or {}) do
      if node.status == "running" or node.status == "pending" then
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
  return shallow_merge(state, { dag_runs = new_runs })
end

function M.run_complete(state, run_id, status, results, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    if type(results) == "table" then
      for node_id, entry in pairs(results) do
        if type(entry) == "table" and entry.skipped == true then
          nodes[node_id] = {
            reasoner = nodes[node_id] and nodes[node_id].reasoner or "",
            status = "skipped",
            started_at_ms = nodes[node_id] and nodes[node_id].started_at_ms or now_ms,
            finished_at_ms = now_ms,
          }
        end
      end
    end
    return shallow_merge(prev, {
      nodes = nodes, completed_at_ms = now_ms, status = status,
    })
  end)
end

-- ── MAG actor-kernel lifecycle ────────────────────────────────────────
--
-- The kernel's `mag.*` event stream drives the same run panel the
-- reasoner-graph `dag.*`/`graph.*` events do, so a kernel run is visible
-- the same way. Actors are the panel's "nodes": spawned → pending,
-- ready → running, killed → killed (distinct), run-complete → the run
-- finalises (still-live actors flip to done). Only `mag.run_started`
-- carries the run identity; the reducer keys every later actor/mod event
-- to the single in-flight run it stamps.

function M.mag_run_started(state, run_id, run_name, now_ms)
  if state.dag_runs and state.dag_runs[run_id] then return state end
  return apply(state, run_id, function(_)
    return {
      run_id = run_id, run_name = run_name, label = "MAG",
      total_nodes = 0, started_at_ms = now_ms, nodes = {},
      completed_at_ms = nil, status = nil, rejected = 0, noops = 0,
      actor_seq = 0,
    }
  end)
end

function M.actor_spawned(state, run_id, actor_id, factory, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
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

function M.actor_ready(state, run_id, actor_id, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    -- A ready without a prior spawn observation (out-of-order tail) still
    -- surfaces as a running node rather than being dropped.
    local node = nodes[actor_id] or { reasoner = "", started_at_ms = now_ms }
    local actor_seq = prev.actor_seq or 0
    local seq = node.seq
    if seq == nil then
      actor_seq = actor_seq + 1
      seq = actor_seq
    end
    nodes[actor_id] = shallow_merge(node, {
      status = "running",
      started_at_ms = node.started_at_ms or now_ms,
      seq = seq,
    })
    return shallow_merge(prev, { nodes = nodes, actor_seq = actor_seq })
  end)
end

function M.actor_killed(state, run_id, actor_id, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]
      and state.dag_runs[run_id].nodes
      and state.dag_runs[run_id].nodes[actor_id]) then
    return state
  end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    nodes[actor_id] = shallow_merge(nodes[actor_id], {
      status = "killed", finished_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

function M.modification_rejected(state, run_id)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    return shallow_merge(prev, { rejected = (prev.rejected or 0) + 1 })
  end)
end

function M.modification_noop(state, run_id)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    return shallow_merge(prev, { noops = (prev.noops or 0) + 1 })
  end)
end

-- Terminal completion for a kernel run. There is no per-actor "done" event
-- in the stream, so every still-live actor flips to done here; a killed
-- actor keeps its terminal state. Stamps `completed_at_ms` so the linger +
-- prune path fades the run out exactly like a reasoner-graph completion.
function M.mag_run_complete(state, run_id, status, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do
      if v.status == "running" or v.status == "pending" then
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

return M
