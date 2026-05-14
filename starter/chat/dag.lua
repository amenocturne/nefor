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
}

local NODE_STYLE = {
  pending = STYLE.dag_pending,
  running = STYLE.dag_running,
  done    = STYLE.dag_done,
  error   = STYLE.dag_error,
  skipped = STYLE.dag_skipped,
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

function M.prune(dag_runs, now_ms)
  if dag_runs == nil then return {} end
  local pruned = nil
  for run_id, run in pairs(dag_runs) do
    local drop = false
    if run.completed_at_ms ~= nil
       and (now_ms - run.completed_at_ms) > M.LINGER_MS then
      drop = true
    end
    if drop then
      if pruned == nil then
        pruned = {}
        for k, v in pairs(dag_runs) do pruned[k] = v end
      end
      pruned[run_id] = nil
    end
  end
  return pruned or dag_runs
end

function M.any_active(dag_runs)
  if type(dag_runs) ~= "table" then return false end
  for _, run in pairs(dag_runs) do
    if run.completed_at_ms == nil then return true end
  end
  return false
end

local function run_header(run)
  local short = run.run_id and run.run_id:sub(1, 8) or "?"
  local total = run.total_nodes or 0
  local nodes = run.nodes or {}
  local done = 0
  local nodes_count = 0
  for _, n in pairs(nodes) do
    nodes_count = nodes_count + 1
    if n.status == "done" or n.status == "error" or n.status == "skipped" then
      done = done + 1
    end
  end
  if nodes_count > total then total = nodes_count end
  local title = string.format("DAG %s (%d/%d)", short, done, total)
  return tui.text { content = title, style = STYLE.footer, wrap = "none" }
end

local function node_row(node_id, node, now_ms, narrow)
  local glyph = GLYPHS[node.status] or "·"
  local style = NODE_STYLE[node.status] or STYLE.status_dim
  local elapsed
  if node.status == "running" then
    elapsed = now_ms - (node.started_at_ms or now_ms)
  elseif node.status == "done" or node.status == "error" then
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
  return tui.text { content = text, style = style, wrap = "none" }
end

local function panel_children(state, now_ms, narrow)
  local children = {}
  local run_ids = sorted_keys(state.dag_runs)
  for i, run_id in ipairs(run_ids) do
    if i > 1 then
      children[#children + 1] = tui.text { content = "", wrap = "none" }
    end
    local run = state.dag_runs[run_id]
    children[#children + 1] = run_header(run)
    local node_ids = sorted_keys(run.nodes or {})
    for _, node_id in ipairs(node_ids) do
      children[#children + 1] = node_row(node_id, run.nodes[node_id], now_ms, narrow)
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

------------------------------------------------------------------------
-- state mutators
------------------------------------------------------------------------

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

return M
