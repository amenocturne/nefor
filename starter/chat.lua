-- starter/chat.lua — chat surface as a Lua composition over tui.* primitives.
--
-- Phase 6 of the nefor-tui rewrite (see specs/nefor-tui-declarative-spec.md).
-- The previous architecture had two Rust plugins for the chat surface
-- (`nefor-chat` owning chat-state, `nefor-tui` owning grid rendering, talking
-- via a grid protocol). This file replaces both in ~one screenful of Lua —
-- demonstrating the opinionation ladder: the Rust plugin is opinion-free,
-- styling and layout decisions live here.
--
-- Loaded by the new `nefor-tui` plugin via `--script chat.lua`. Inside the
-- plugin's Lua VM the only globals are `tui.*` (primitive constructors +
-- emit/send_to/scroll_* helpers); JSON conversion happens at the Rust↔Lua
-- bridge so Lua tables ARE the chat-contract bodies.
--
-- Wire shape (chat-contract v0.1, see docs/chat-contract.md):
--
--   inbound  → chat.message.append, chat.stream.delta, chat.stream.end,
--              chat.session.stats, chat.tool.start, chat.tool.end,
--              chat.popup, chat.auth.status, chat.model.set_ack
--   outbound ← chat.input.submit, chat.interrupt, chat.interrupt_all,
--              chat.reset
--
-- v1 deferrals (deliberate, documented):
--   * No /resume / chat.history.replay restore — phase 7.
--   * No reasoning row collapse — Qwen-specific UX, deferred.
--   * No slash autocomplete dropdown — keep one screenful for v1.
--   * Cost color ramp deferred; statusline shows raw stats.
--
-- Phase 7 (DAG panel): the Ctrl+B sidebar now subscribes to graph.*
-- events from `reasoner-graph` and draws one row per tracked run header
-- + one row per node with status glyph + elapsed time. Linger handling
-- is pure-update pruning (no engine timer API): every update reads
-- `tui.now_ms()` and drops any run whose `completed_at_ms + 2s < now`.
-- That works because the engine pushes its frame-clock into Lua before
-- every dispatch, so subsequent events drive the prune cycle. A run
-- that completes during a dead-quiet moment still lingers a hair longer
-- than 2s — it's stamped on the next event, which is fine: the user has
-- already seen the green/red completion marker.

------------------------------------------------------------------------
-- helpers
------------------------------------------------------------------------

-- Functional list helpers — chat.lua's view builders consume `state.entries`
-- as a list and produce a list of widgets, so map+filter close the loop
-- without a single explicit for-loop in the view layer.
local function map(list, fn)
  local out = {}
  for i, v in ipairs(list) do out[i] = fn(v, i) end
  return out
end

local function shallow_merge(a, b)
  local out = {}
  for k, v in pairs(a) do out[k] = v end
  for k, v in pairs(b) do out[k] = v end
  return out
end

-- Drop trailing nils from a list (so child arrays with conditional
-- entries don't break tui.column's #children iteration when an entry is
-- nil). Lua's table-as-list semantics make `{ a, nil, c }` == `{ a }`,
-- which is fine, but `{ a, nil_or_widget, c }` may surprise — explicit
-- compaction makes the intent obvious at the call site.
local function compact(list)
  local out = {}
  for _, v in ipairs(list) do
    if v ~= nil then out[#out + 1] = v end
  end
  return out
end

------------------------------------------------------------------------
-- styling (the only place opinion lives)
------------------------------------------------------------------------
--
-- Themes here are Lua tables, neutral by default in primitives. Edit at
-- will. The Rust plugin ships zero defaults — the visual identity of the
-- chat surface is fully redefinable in this file.

-- ANSI 256-color indices picked for legibility on dark + light terminals.
-- Index reference: https://en.wikipedia.org/wiki/ANSI_escape_code#8-bit
-- (2, 10 = green; 4, 12 = blue; 5, 13 = magenta; 3, 11 = yellow;
--  6, 14 = cyan; 1, 9 = red; 8 = bright black / "gray").
local C = {
  green   = 10,
  blue    = 12,
  magenta = 13,
  yellow  = 11,
  cyan    = 14,
  red     = 9,
  gray    = 8,
}

-- Style records. Schema: { fg, bg, bold, italic, underline, reverse } —
-- all fields optional; missing fields fall through to neutral. The Rust
-- plugin ships zero defaults, so this table is the entire visual identity
-- of the chat surface.
local STYLE = {
  user_label      = { fg = C.blue,    bold = true },
  assistant_label = { fg = C.magenta, bold = true },
  system_label    = { fg = C.yellow,  italic = true },
  tool_label      = { fg = C.cyan },
  tool_error      = { fg = C.red, bold = true },
  status_dim      = { fg = C.gray },
  input_border    = { fg = C.gray },
  popup_border    = { fg = C.yellow },
  -- DAG panel — one style per node lifecycle bucket. The glyph and the
  -- status word share the same fg so a row reads as green/amber/red
  -- end-to-end at a glance.
  dag_header      = { fg = C.gray },
  dag_separator   = { fg = C.gray },
  dag_pending     = { fg = C.gray },
  dag_running     = { fg = C.yellow },
  dag_done        = { fg = C.green },
  dag_error       = { fg = C.red, bold = true },
  dag_skipped     = { fg = C.gray, italic = true },
}

-- Markdown theme — passed as `theme` to `tui.markdown`. Neutral entries
-- (or missing keys) mean "fall through to plain text"; supplied entries
-- get highlighting.
local MARKDOWN_THEME = {
  bold        = { bold = true },
  italic      = { italic = true },
  code        = { fg = C.yellow },
  code_block  = { fg = C.yellow },
  h1          = { fg = C.magenta, bold = true },
  h2          = { fg = C.magenta, bold = true },
  h3          = { fg = C.magenta },
  link        = { fg = C.blue, underline = true },
  blockquote  = { fg = C.gray, italic = true },
  list_marker = { fg = C.cyan },
}

------------------------------------------------------------------------
-- state shape
------------------------------------------------------------------------
--
--   entries           list of { role, text, model?, duration_ms?, kind }
--                     where role ∈ { "user", "assistant", "system", "tool" }
--                     and kind ∈ { "text", "stream", "tool_call" }
--   in_flight         index into entries of the in-flight assistant entry
--                     (or nil); reset on chat.stream.end
--   input_value       text_input current value (controlled component)
--   focused_id        which keyed widget claims keystrokes ("input")
--   show_sidebar      Ctrl+B toggle (DAG panel visibility)
--   popup             nil | { title, body } — Ctrl+O help, system errors
--   stats             { model?, prompt_tokens?, completion_tokens?, cost_usd?,
--                       turns?, duration_ms? } — populated from chat.session.stats
--   pending           true while awaiting first delta of a turn
--   model             active provider model string (chat.model.set_ack)
--   dag_runs          map keyed by run_id, each entry shape:
--                       { run_id, total_nodes, started_at_ms,
--                         completed_at_ms?, status?,
--                         nodes = { [node_id] = {
--                            reasoner, status, started_at_ms,
--                            finished_at_ms? } } }
--                     status ∈ { "pending","running","done","error","skipped" }
--                     for nodes; for runs the wire status is recorded as-is
--                     (`success` / `partial_failure` / `failure`).
--                     Updated by graph.* events; pruned in update once a run
--                     has been completed for >= DAG_LINGER_MS.

-- How long a finished DAG run lingers in the panel after `graph.run_complete`
-- before pure-update pruning drops it. 2s is the same window the legacy
-- nefor-chat plugin used — short enough to avoid stale panels and long
-- enough that the user sees the final green/red marker as confirmation
-- the run actually finished.
local DAG_LINGER_MS = 2000

local function initial_state()
  return {
    entries      = {},
    in_flight    = nil,
    input_value  = "",
    focused_id   = "input",
    show_sidebar = false,
    popup        = nil,
    stats        = {},
    pending      = false,
    model        = nil,
    dag_runs     = {},
  }
end

------------------------------------------------------------------------
-- entry rendering
------------------------------------------------------------------------

-- Role label widget — colored single-row prefix above each entry's body.
-- Pure function: same input → same output, no side effects, no state.
local function role_label(role)
  local m = {
    user      = { text = "you",       style = STYLE.user_label },
    assistant = { text = "assistant", style = STYLE.assistant_label },
    system    = { text = "system",    style = STYLE.system_label },
    tool      = { text = "tool",      style = STYLE.tool_label },
  }
  local cfg = m[role] or { text = role, style = nil }
  return tui.text { content = cfg.text, style = cfg.style }
end

-- Render one transcript entry. Branches by `entry.kind`:
--   text       — plain `tui.text` (user input, system messages).
--   stream     — `tui.markdown` (assistant content; reflows on every
--                stream.delta because state.entries[i].text changed).
--   tool_call  — `▸ name(args) → output` one-liner (collapsed). Phase 7
--                will add expand-on-Ctrl+O.
local function render_entry(entry)
  if entry.kind == "tool_call" then
    local prefix = entry.error and "✗ " or "▸ "
    local style = entry.error and STYLE.tool_error or STYLE.tool_label
    local line = prefix .. (entry.name or "?")
    if entry.input and #entry.input > 0 then
      -- Truncate huge tool args so a giant JSON blob doesn't dominate
      -- the transcript; the full payload lives in the unstyled `entry.input`
      -- slot and a phase-7 expand toggle will surface it.
      local trimmed = entry.input
      if #trimmed > 80 then trimmed = trimmed:sub(1, 77) .. "..." end
      line = line .. "(" .. trimmed .. ")"
    end
    if entry.output and #entry.output > 0 then
      local trimmed = entry.output
      if #trimmed > 60 then trimmed = trimmed:sub(1, 57) .. "..." end
      line = line .. " → " .. trimmed
    end
    return tui.text { content = line, style = style, wrap = "word" }
  end

  if entry.kind == "stream" or entry.role == "assistant" then
    return tui.column {
      gap = 0,
      children = {
        role_label(entry.role),
        tui.markdown {
          source = entry.text or "",
          theme  = MARKDOWN_THEME,
          wrap   = "word",
        },
      },
    }
  end

  -- Default: plain text body (user, system, etc).
  return tui.column {
    gap = 0,
    children = {
      role_label(entry.role),
      tui.text { content = entry.text or "", wrap = "word" },
    },
  }
end

------------------------------------------------------------------------
-- statusline
------------------------------------------------------------------------
--
-- A single row at the bottom of the chrome above the input. Shows:
--   model · in/out tokens · cost · turns · duration
-- Missing fields render as "—" — partial provider data is the common case.

local function fmt_or_dash(v, fmt)
  if v == nil then return "—" end
  if fmt then return string.format(fmt, v) end
  return tostring(v)
end

local function statusline(state)
  local s = state.stats or {}
  local segments = {
    "model: " .. (state.model or s.model or "—"),
    "in: "    .. fmt_or_dash(s.prompt_tokens),
    "out: "   .. fmt_or_dash(s.completion_tokens),
    "cost: $" .. fmt_or_dash(s.cost_usd, "%.4f"),
    "turns: " .. fmt_or_dash(s.turns),
    "Δt: "    .. (s.duration_ms and (tostring(math.floor(s.duration_ms / 1000)) .. "s") or "—"),
  }
  if state.pending then
    segments[#segments + 1] = "[thinking…]"
  end
  return tui.text {
    content = table.concat(segments, "  ·  "),
    style   = STYLE.status_dim,
    wrap    = "none",
  }
end

------------------------------------------------------------------------
-- popup (help on Ctrl+O, error popups from chat.popup events)
------------------------------------------------------------------------

local HELP_TEXT = [[Keys:
  Enter        send message
  Esc          cancel current turn
  Esc Esc      cancel everything (double-tap)
  Ctrl+B       toggle sidebar
  Ctrl+O       toggle this help
  PgUp / PgDn  scroll transcript
  Home / End   jump to top / bottom
  Ctrl+C       quit

Slash commands:
  /new         new chat (clears transcript)
  /quit        exit nefor

Phase 6 cutover: the chat surface is a ~280-LOC Lua composition.
Tweak starter/chat.lua to taste.]]

local function popup_widget(state)
  if not state.popup then return nil end
  return tui.anchored {
    anchor = "center",
    width  = "60%",
    height = "60%",
    child  = tui.padding {
      value = 1,
      child = tui.column {
        gap = 1,
        children = {
          tui.text {
            content = state.popup.title or "info",
            style   = STYLE.popup_border,
          },
          tui.markdown {
            source = state.popup.body or "",
            theme  = MARKDOWN_THEME,
            wrap   = "word",
          },
          tui.text { content = "(press Esc or Ctrl+O to close)", style = STYLE.status_dim },
        },
      },
    },
  }
end

------------------------------------------------------------------------
-- DAG panel (sidebar widget — phase 7)
------------------------------------------------------------------------
--
-- Renders state.dag_runs as a stack of run sections. Each section has
-- a one-line header with the abbreviated run id and `<done>/<total>`
-- counter, then one row per node showing:
--
--     <glyph> <node_id>  <reasoner?>  <status_word> <elapsed>
--
-- The reasoner column is dropped on narrow sidebars so 28-col layouts
-- still fit `<glyph> <node> <elapsed>` without truncating into uselessness.

local DAG_GLYPHS = {
  pending = "○",
  running = "●",
  done    = "✓",
  error   = "✗",
  skipped = "⊘",
}

local DAG_NODE_STYLE = {
  pending = STYLE.dag_pending,
  running = STYLE.dag_running,
  done    = STYLE.dag_done,
  error   = STYLE.dag_error,
  skipped = STYLE.dag_skipped,
}

-- Stable lexicographic order over a string-keyed table — Lua's pairs is
-- hash-iterated so rendering would jitter without an explicit sort.
local function sorted_keys(map)
  local out = {}
  for k in pairs(map) do out[#out + 1] = k end
  table.sort(out)
  return out
end

local function fmt_elapsed_ms(ms)
  if ms == nil then return "" end
  if ms < 1000 then
    return tostring(ms) .. "ms"
  end
  return string.format("%.1fs", ms / 1000)
end

-- Drop runs whose linger window has expired. Returns the same map ref
-- when nothing changes (so update can compare-and-skip), or a fresh map
-- with the stale entries removed. Pure-update pruning: no engine timer
-- needed — the next event drives this and the visible-but-stale row
-- disappears on the following frame.
local function prune_dag_runs(dag_runs, now_ms)
  if dag_runs == nil then return {} end
  local pruned = nil
  for run_id, run in pairs(dag_runs) do
    if run.completed_at_ms ~= nil
       and (now_ms - run.completed_at_ms) > DAG_LINGER_MS then
      if pruned == nil then
        pruned = {}
        for k, v in pairs(dag_runs) do pruned[k] = v end
      end
      pruned[run_id] = nil
    end
  end
  return pruned or dag_runs
end

-- Header row: "DAG <prefix> (M/N)" rendered in dim chrome.
local function dag_run_header(run)
  local short = run.run_id and run.run_id:sub(1, 8) or "?"
  local total = run.total_nodes or 0
  local nodes = run.nodes or {}
  local done = 0
  for _, n in pairs(nodes) do
    if n.status == "done" or n.status == "error" or n.status == "skipped" then
      done = done + 1
    end
  end
  -- Use the larger of total_nodes and observed-node count so a synthetic
  -- run created from an out-of-order node_dispatched still shows a sane
  -- counter rather than (1/0).
  local nodes_count = 0
  for _ in pairs(nodes) do nodes_count = nodes_count + 1 end
  if nodes_count > total then total = nodes_count end
  local title = string.format("DAG %s (%d/%d)", short, done, total)
  return tui.text { content = title, style = STYLE.dag_header, wrap = "none" }
end

-- One node row inside a run section.
local function dag_node_row(node_id, node, now_ms, narrow)
  local glyph = DAG_GLYPHS[node.status] or "·"
  local style = DAG_NODE_STYLE[node.status] or STYLE.status_dim
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
    -- `<glyph> <node_id> <elapsed>` — no reasoner column, no status word.
    text = glyph .. " " .. node_id .. elapsed_str
  else
    -- `<glyph> <node_id>  <reasoner>  <status>[ <elapsed>]`. Two-space
    -- gutters keep the columns readable; styling is uniform per row so
    -- the green/amber/red signal carries even without per-segment colour
    -- (text/spans split would force us to pre-pad each column to its
    -- width here in Lua — not worth the complexity for v1).
    local reasoner = node.reasoner or ""
    local status_word = node.status or "?"
    text = string.format("%s %s  %s  %s%s",
      glyph, node_id, reasoner, status_word, elapsed_str)
  end
  return tui.text { content = text, style = style, wrap = "none" }
end

-- Build one section per run: header + one row per node, separated by a
-- single blank line between runs. The whole stack is then capped to a
-- "+K more" overflow row in the sidebar's outer scroll budget — but for
-- v1 we just emit everything; the sidebar's `constrained` widget keeps
-- the column from blowing out and the chat's column gives whatever rows
-- the terminal has left.
local function dag_panel_children(state, now_ms, narrow)
  local children = {}
  local run_ids = sorted_keys(state.dag_runs)
  for i, run_id in ipairs(run_ids) do
    if i > 1 then
      children[#children + 1] = tui.text {
        content = "",
        style   = STYLE.dag_separator,
        wrap    = "none",
      }
    end
    local run = state.dag_runs[run_id]
    children[#children + 1] = dag_run_header(run)
    local node_ids = sorted_keys(run.nodes or {})
    for _, node_id in ipairs(node_ids) do
      children[#children + 1] = dag_node_row(node_id, run.nodes[node_id], now_ms, narrow)
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

local function dag_panel(state)
  -- Width budget mirrors the legacy plugin's heuristic: ~36 cols is
  -- enough for the wide layout, narrower terminals fall back to compact.
  -- The constrained widget below caps at 36, so we always render compact
  -- when the sidebar is at its lower clamp.
  local narrow = true   -- v1: always compact; phase-7+ can detect width
                        -- via tui.scroll_position-style geometry hooks.
  local now_ms = tui.now_ms()
  return tui.constrained {
    min_width = 28,
    max_width = 36,
    child = tui.padding {
      value = 1,
      child = tui.column {
        gap      = 0,
        children = dag_panel_children(state, now_ms, narrow),
      },
    },
  }
end

-- Vertical separator column between transcript and sidebar. The text
-- primitive renders a single glyph; layout repeats the column across
-- the full row height because constrained's `min_height` defaults open
-- and `expanded`'s `child` paints the glyph at the top — so we keep the
-- separator visually anchored to the top row, which reads as a clean
-- divider against the transcript's right edge for v1.
local function vertical_separator()
  return tui.constrained {
    min_width = 1,
    max_width = 1,
    child = tui.text {
      content = "│",
      style   = STYLE.dag_separator,
      wrap    = "none",
    },
  }
end

------------------------------------------------------------------------
-- view
------------------------------------------------------------------------

local function transcript(state)
  local children = map(state.entries, render_entry)
  return tui.scrollable {
    key       = "transcript",
    stick_to  = "end",
    scrollbar = "auto",
    child     = tui.column { gap = 1, children = children },
  }
end

local function view(state)
  local body_row = tui.row {
    gap = 0,
    children = compact {
      tui.expanded { child = transcript(state) },
      state.show_sidebar and vertical_separator() or nil,
      state.show_sidebar and dag_panel(state)        or nil,
    },
  }

  return tui.stack {
    children = compact {
      tui.column {
        gap = 0,
        children = {
          tui.expanded { child = body_row },
          statusline(state),
          tui.text_input {
            key       = "input",
            value     = state.input_value,
            focused   = state.focused_id == "input",
            on_change = "input.changed",
            on_submit = "input.submit",
            min_lines = 1,
            max_lines = 6,
            placeholder = "type a message — Enter to send, /quit to exit",
          },
        },
      },
      popup_widget(state),
    },
  }
end

------------------------------------------------------------------------
-- transcript helpers (state mutation, kept local to chat.lua)
------------------------------------------------------------------------

local function push_entry(state, entry)
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  entries[#entries + 1] = entry
  return shallow_merge(state, { entries = entries })
end

-- Append delta text into the in-flight assistant entry. If no entry is
-- in flight, create one. Mirrors the legacy chat plugin's
-- `append_assistant_delta` semantics.
local function append_assistant_delta(state, delta)
  if state.in_flight ~= nil and state.entries[state.in_flight] then
    local entries = {}
    for i, v in ipairs(state.entries) do
      entries[i] = (i == state.in_flight)
        and shallow_merge(v, { text = (v.text or "") .. delta })
        or v
    end
    return shallow_merge(state, { entries = entries, pending = false })
  end
  -- First delta of a new turn.
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  entries[#entries + 1] = {
    role = "assistant",
    text = delta,
    kind = "stream",
  }
  return shallow_merge(state, {
    entries   = entries,
    in_flight = #entries,
    pending   = false,
  })
end

local function finalize_assistant(state, final_text, model, duration_ms)
  if state.in_flight == nil then return state end
  local entries = {}
  for i, v in ipairs(state.entries) do
    if i == state.in_flight then
      local merged = shallow_merge(v, {
        text        = final_text and #final_text > 0 and final_text or v.text,
        model       = model or v.model,
        duration_ms = duration_ms or v.duration_ms,
      })
      entries[i] = merged
    else
      entries[i] = v
    end
  end
  return shallow_merge(state, {
    entries   = entries,
    in_flight = nil,
    pending   = false,
  })
end

local function attach_tool_end(state, id, output, error_flag)
  local entries = {}
  local matched = false
  for i, v in ipairs(state.entries) do
    if not matched and v.kind == "tool_call" and v.id == id then
      entries[i] = shallow_merge(v, { output = output or "", error = error_flag })
      matched = true
    else
      entries[i] = v
    end
  end
  if not matched then return state end
  return shallow_merge(state, { entries = entries })
end

------------------------------------------------------------------------
-- update
------------------------------------------------------------------------
--
-- The router for every event reaching the surface — keys, mouse,
-- text_input callbacks, NCP envelopes from peers (chat.* + others). All
-- state changes flow through here; side-effects are returned as a list
-- the engine drains.

local function parse_slash(text)
  if text:sub(1, 1) ~= "/" then return nil end
  local cmd = text:match("^/(%S+)")
  return cmd
end

-- ── DAG-panel state mutators ──────────────────────────────────────────
--
-- Each handler returns a fresh state with state.dag_runs replaced by an
-- updated map. Treating dag_runs as immutable per dispatch matches the
-- shallow_merge convention the rest of the surface uses: no aliasing
-- across frames means the reconciler diffs cleanly.

local function dag_apply(state, run_id, fn)
  local prev_runs = state.dag_runs or {}
  local new_runs  = {}
  for k, v in pairs(prev_runs) do new_runs[k] = v end
  new_runs[run_id] = fn(prev_runs[run_id])
  return shallow_merge(state, { dag_runs = new_runs })
end

local function dag_run_started(state, run_id, total_nodes, now_ms)
  if state.dag_runs and state.dag_runs[run_id] then return state end
  return dag_apply(state, run_id, function(_)
    return {
      run_id          = run_id,
      total_nodes     = total_nodes or 0,
      started_at_ms   = now_ms,
      nodes           = {},
      completed_at_ms = nil,
      status          = nil,
    }
  end)
end

local function dag_node_dispatched(state, run_id, node_id, reasoner, now_ms)
  return dag_apply(state, run_id, function(prev)
    local run = prev or {
      run_id          = run_id,
      total_nodes     = 0,
      started_at_ms   = now_ms,
      nodes           = {},
      completed_at_ms = nil,
    }
    local nodes = {}
    for k, v in pairs(run.nodes or {}) do nodes[k] = v end
    nodes[node_id] = {
      reasoner       = reasoner or "",
      status         = "running",
      started_at_ms  = now_ms,
      finished_at_ms = nil,
    }
    return shallow_merge(run, { nodes = nodes })
  end)
end

local function dag_node_result(state, run_id, node_id, has_output, has_error, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]
          and state.dag_runs[run_id].nodes
          and state.dag_runs[run_id].nodes[node_id]) then
    return state
  end
  return dag_apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[node_id]
    local status
    if has_output    then status = "done"
    elseif has_error then status = "error"
    else                  status = "error" -- malformed; mark as error
    end
    nodes[node_id] = shallow_merge(node, {
      status         = status,
      finished_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

local function dag_run_complete(state, run_id, status, results, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return dag_apply(state, run_id, function(prev)
    -- Apply terminal status to nodes that didn't see a node_result —
    -- skipped nodes appear as `{ skipped = true }` in `results`. Keeps
    -- the panel honest about what actually ran.
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    if type(results) == "table" then
      for node_id, entry in pairs(results) do
        if type(entry) == "table" and entry.skipped == true then
          nodes[node_id] = {
            reasoner       = nodes[node_id] and nodes[node_id].reasoner or "",
            status         = "skipped",
            started_at_ms  = nodes[node_id] and nodes[node_id].started_at_ms or now_ms,
            finished_at_ms = now_ms,
          }
        end
      end
    end
    return shallow_merge(prev, {
      nodes           = nodes,
      completed_at_ms = now_ms,
      status          = status,
    })
  end)
end

local function update(msg, state)
  local kind = msg.kind or ""

  -- Pure-update prune: every event drives a stale-run sweep so a
  -- completed run drops within DAG_LINGER_MS of any subsequent dispatch
  -- (key, mouse, peer event — anything). When the panel is visible and
  -- a run just completed there's nearly always more traffic; in the
  -- rare quiescent case the row sticks around until the next event,
  -- which the user already saw transition green/red.
  do
    local now = tui.now_ms()
    local pruned = prune_dag_runs(state.dag_runs or {}, now)
    if pruned ~= state.dag_runs then
      state = shallow_merge(state, { dag_runs = pruned })
    end
  end

  -- ── text_input callbacks ────────────────────────────────────────────
  if kind == "input.changed" then
    return shallow_merge(state, { input_value = msg.value or "" }), {}
  end

  if kind == "input.submit" then
    local text = msg.value or ""
    if #text == 0 then return state, {} end
    -- Slash commands handled locally; everything else ships as
    -- chat.input.submit on the bus.
    local slash = parse_slash(text)
    if slash == "quit" or slash == "exit" then
      return state, { { kind = "exit" } }
    end
    if slash == "new" then
      local new_state = shallow_merge(state, {
        entries     = {},
        in_flight   = nil,
        input_value = "",
        pending     = false,
      })
      return new_state, {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.reset" } },
      }
    end
    -- Echo the user message into the transcript so the entry shows up
    -- before the network round-trip; mirrors legacy chat plugin.
    local with_user = push_entry(state, { role = "user", text = text, kind = "text" })
    local cleared = shallow_merge(with_user, {
      input_value = "",
      pending     = true,
    })
    return cleared, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = text } },
    }
  end

  -- ── keyboard shortcuts (all bubble unless a text_input swallowed) ──
  if kind == "key.ctrl_c" then
    return state, { { kind = "exit" } }
  end

  if kind == "key.ctrl_b" then
    return shallow_merge(state, { show_sidebar = not state.show_sidebar }), {}
  end

  if kind == "key.ctrl_o" then
    if state.popup then
      return shallow_merge(state, { popup = nil }), {}
    end
    return shallow_merge(state, {
      popup = { title = "help", body = HELP_TEXT },
    }), {}
  end

  if kind == "key.escape" then
    if state.popup then
      return shallow_merge(state, { popup = nil }), {}
    end
    if state.pending or state.in_flight ~= nil then
      return state, {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt" } },
      }
    end
    return state, {}
  end

  if kind == "key.pageup" then
    tui.scroll_by("transcript", -10)
    return state, {}
  end
  if kind == "key.pagedown" then
    tui.scroll_by("transcript", 10)
    return state, {}
  end
  if kind == "key.home" then
    tui.scroll_to("transcript", 0)
    return state, {}
  end
  if kind == "key.end" then
    tui.scroll_into_view("transcript")
    return state, {}
  end

  -- ── inbound chat-contract events ────────────────────────────────────
  if kind == "chat.message.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    return push_entry(state, {
      role = msg.role or "system",
      text = text,
      kind = "text",
    }), {}
  end

  if kind == "chat.stream.delta" then
    local t = msg.text or msg.delta or ""
    if #t == 0 then return state, {} end
    return append_assistant_delta(state, t), {}
  end

  if kind == "chat.stream.end" then
    local final = msg.text
    return finalize_assistant(state, final, msg.model, msg.duration_ms), {}
  end

  if kind == "chat.session.stats" then
    local stats = shallow_merge(state.stats or {}, {})
    for k, v in pairs(msg) do
      if k ~= "kind" then stats[k] = v end
    end
    return shallow_merge(state, { stats = stats }), {}
  end

  if kind == "chat.tool.start" then
    return push_entry(state, {
      kind   = "tool_call",
      role   = "tool",
      id     = msg.id or "",
      name   = msg.name or "?",
      input  = msg.input and (type(msg.input) == "string"
                              and msg.input
                              or "(object)") or "",
    }), {}
  end

  if kind == "chat.tool.end" then
    return attach_tool_end(state, msg.id or "", msg.output or "", msg.error == true), {}
  end

  if kind == "chat.popup" then
    return shallow_merge(state, {
      popup = {
        title = msg.title or msg.level or "popup",
        body  = msg.message or msg.text or "",
      },
    }), {}
  end

  if kind == "chat.model.set_ack" then
    return shallow_merge(state, { model = msg.model or state.model }), {}
  end

  if kind == "chat.auth.status" then
    -- Phase 6 keeps auth handling minimal; surface a system entry so the
    -- user sees something when an /login flow lands. Phase 7 can render
    -- it in the statusline once the auth panel design firms up.
    if msg.status and msg.status ~= "ok" then
      return push_entry(state, {
        role = "system",
        text = "[auth " .. tostring(msg.status) .. "] " .. tostring(msg.message or ""),
        kind = "text",
      }), {}
    end
    return state, {}
  end

  -- ── DAG observation (reasoner-graph plugin lifecycle events) ────────
  --
  -- The chat surface is a passive observer of every graph.* event on
  -- the bus — it doesn't submit graphs itself; it just renders what's
  -- in flight. The four lifecycle events form a state machine:
  --
  --   run_started → node_dispatched → node_result → run_complete
  --
  -- with run_complete optionally arriving without per-node results when
  -- the run aborts before dispatch (typecheck failure, etc).
  if kind == "graph.run_started" then
    local now = tui.now_ms()
    return dag_run_started(state, msg.run_id or "", msg.total_nodes or 0, now), {}
  end

  if kind == "graph.node_dispatched" then
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    return dag_node_dispatched(state, msg.run_id, msg.node_id, msg.reasoner or "", now), {}
  end

  if kind == "graph.node_result" then
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    -- The wire encodes terminal status as "output present" vs "error
    -- present"; we don't try to parse the value — the panel just needs
    -- to know which bucket to flip the glyph to.
    local has_output = msg.output ~= nil
    local has_error  = msg.error  ~= nil
    return dag_node_result(state, msg.run_id, msg.node_id, has_output, has_error, now), {}
  end

  if kind == "graph.run_complete" then
    if (msg.run_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    return dag_run_complete(state, msg.run_id, msg.status, msg.results, now), {}
  end

  -- Unrecognised event — log silently (the Rust plugin's tracing layer
  -- catches these at trace level if needed).
  return state, {}
end

------------------------------------------------------------------------
-- start
------------------------------------------------------------------------

tui.start {
  initial_state = initial_state(),
  view          = view,
  update        = update,
}
