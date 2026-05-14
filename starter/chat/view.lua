-- Full chat-surface layout. Composes transcript, input, statusline,
-- DAG sidebar, popups, and toast into a single `tui.stack`. This file
-- is the only place that knows about the overall geometry — every
-- sub-renderer hands back a single node and the layout decides where
-- it goes.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget

local common      = require("chat.common")
local entries_mod = require("chat.entries")
local statusline  = require("chat.statusline")
local dag         = require("chat.dag")
local popups      = require("chat.popups")
local slash       = require("chat.slash")

local STYLE   = common.STYLE
local compact = common.compact
local CURSOR_ROW_STYLE = common.CURSOR_ROW_STYLE

local M = {}

-- Spec section 6: pre-first-delta placeholder is `[thinking... Ns]`,
-- static (no spinner) but with per-second elapsed counter. We
-- piggyback on tui.animation for its frame-rate side effect — it
-- keeps the render loop alive at ~1Hz so the counter advances even
-- without inbound events — but render zero-width frames, so visually
-- there's no spinner.
local THINKING_TICK_FRAMES = { "", "" }

local function thinking_widget(state)
  if not state.pending then return nil end
  if state.in_flight ~= nil then return nil end
  local elapsed_ms = state.turn_started_at and (tui.now_ms() - state.turn_started_at) or 0
  local secs = math.floor(elapsed_ms / 1000)
  local body = secs > 0
    and string.format("[thinking... %ds]", secs)
    or  "[thinking...]"
  return tui.row {
    gap = 0,
    children = {
      tui.animation {
        frames      = THINKING_TICK_FRAMES,
        duration_ms = 1000,
      },
      tui.text { content = body, style = STYLE.system, wrap = "none" },
    },
  }
end

local function transcript(state)
  -- Welcome banner shows on a fresh surface only; the chat widget's
  -- `empty_view` slot accepts a fn returning the banner tree, which
  -- it stacks over an empty scrollable so scroll_position keeps
  -- resolving. Replay-mode opt-out: between sessions.replay.start
  -- and the first replayed chat.message.append, the transcript is
  -- briefly empty AND we're rebuilding. Painting the banner here
  -- would flash the welcome copy in the middle of a resume.
  local empty_view
  if state.in_flight == nil and not state.pending and not state.replay_mode then
    empty_view = statusline.welcome_banner
  end
  return W.chat.view({
    key          = "transcript",
    entries      = function() return state.entries or {} end,
    render_entry = function(e, i)
      return entries_mod.render(e, i, state.expanded_details)
    end,
    append       = thinking_widget(state),
    empty_view   = empty_view,
  })
end

-- Keep the engine's render loop alive at ~1Hz while any per-second
-- elapsed counter is on screen — tui.now_ms() only re-evaluates on a
-- render, and the engine renders only on state changes / animation
-- ticks. Without this, the DAG sidebar's "Ns" stalls between events.
-- Mount only when something needs to refresh.
local KEEPALIVE_FRAMES = { "", "" }

local function render_keepalive(state)
  -- Toast inclusion is load-bearing: without it the engine renders
  -- only on state changes, so the toast appears once and never
  -- re-renders to run its slide-out / disappearance. duration_ms = 100
  -- keeps the toast slide smooth (~60fps engine tick when active);
  -- DAG-elapsed counters only need 1Hz but the extra ticks are free.
  local has_toast = state.toasts and #state.toasts > 0
  if not (state.pending or dag.any_active(state.dag_runs, tui.now_ms()) or has_toast) then
    return nil
  end
  return tui.animation {
    frames      = KEEPALIVE_FRAMES,
    duration_ms = 100,
  }
end

function M.render(state)
  -- The input drops focus while certain popups own the keyboard. Tool
  -- permission expects single-char A/D; model picker takes printable
  -- chars as filter input — both paths require input to stop
  -- swallowing keys.
  local popup_owns_keys = state.popup and (
    state.popup.variant == "tool_permission" or
    state.popup.variant == "model_picker" or
    state.popup.variant == "session_picker"
  )
  local input_focused = not popup_owns_keys
  local input_border_style = input_focused
    and STYLE.input_border
    or STYLE.input_border_unfocused
  -- The prompt widget owns trigger detection + popup rendering + Tab
  -- routing for both slash and @-path completion. Chat.lua declares
  -- the completion sources via slash.completions() and reads the
  -- selected match back from state.completion on submit (Enter
  -- promotes the highlighted slash match to its command text).
  local input_field = W.prompt.view({
    state          = {
      value      = state.input_value,
      completion = state.completion,
    },
    key             = "input",
    focused         = input_focused,
    on_change       = "input.changed",
    on_submit       = "input.submit",
    border_style    = input_border_style,
    border_key      = "input-field",
    min_lines       = 1,
    max_lines       = 6,
    selectable      = true,
    completions     = slash.completions(),
    completions_view = {
      cursor_style = CURSOR_ROW_STYLE,
      empty_style  = STYLE.status_dim,
    },
  })

  -- One-row blank spacer reused at the top of the chat column and the
  -- bottom (above the statusline). The sidebar gets no spacer: its
  -- vertical separator runs full window height edge-to-edge.
  local function blank_row()
    return tui.constrained {
      max_height = 1,
      child = tui.fill { char = " " },
    }
  end

  -- Left column = chat surface. Top → bottom: 1-row top gap /
  -- transcript / input (carries its own autocomplete) / statusline /
  -- 1-row bottom gap / keepalive. Statusline lives BELOW the input —
  -- pushing it above the input visibly inverts the screen weight,
  -- making the input feel like a status row rather than the primary
  -- focus surface. The bottom gap lifts the statusline off the very
  -- last row so it doesn't sit flush against the terminal frame.
  local left_column = tui.column {
    gap = 0,
    children = compact {
      blank_row(),
      tui.expanded { child = transcript(state) },
      input_field,
      statusline.view(state),
      blank_row(),
      render_keepalive(state),
    },
  }

  -- Outer row: left column (chat) | separator | sidebar. No outer
  -- padding — the sidebar's vertical separator reaches the full
  -- window height (top and bottom edges flush), and per-element
  -- spacing is handled inside left_column and dag.panel.
  local main_row = tui.row {
    gap = 0,
    children = compact {
      tui.expanded { child = left_column },
      state.show_sidebar and dag.vertical_separator() or nil,
      state.show_sidebar and dag.panel(state)         or nil,
    },
  }

  return tui.stack {
    children = compact {
      main_row,
      popups.help(state),
      popups.message(state),
      popups.model_picker(state),
      popups.session_picker(state),
      popups.tool_permission(state),
      -- Toast renders last so it sits above input, statusline, and
      -- every popup — non-blocking notifications must never be
      -- occluded by chrome below them.
      W.toast.view({ toasts = state.toasts }),
    },
  }
end

return M
