-- Full chat-surface layout. Composes transcript, input, statusline,
-- run-panel sidebar, popups, and toast into a single `tui.stack`. This file
-- is the only place that knows about the overall geometry — every
-- sub-renderer hands back a single node and the layout decides where
-- it goes.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget

local common      = require("libs.chat.common")
local entries_mod = require("libs.chat.entries")
local queued_input = require("libs.chat.queued_input")
local run_panel   = require("libs.chat.run_panel")
local popups      = require("libs.chat.popups")

local STYLE   = common.STYLE
local compact = common.compact
local CURSOR_ROW_STYLE = common.CURSOR_ROW_STYLE

local M = {}

-- Pre-first-delta placeholder is `[thinking... Ns]`, static (no
-- spinner) but with per-second elapsed counter. We piggyback on
-- tui.animation for its frame-rate side effect — it keeps the render
-- loop alive at ~1Hz so the counter advances even without inbound
-- events — but render zero-width frames, so visually there's no
-- spinner.
local THINKING_TICK_FRAMES = { "", "" }

local function thinking_widget(state)
  if not state.pending then return nil end
  if state.in_flight ~= nil then return nil end
  local elapsed_ms = state.turn_started_at and (tui.now_ms() - state.turn_started_at) or 0
  local duration = common.humanize_duration_ms(elapsed_ms)
  local body = elapsed_ms >= 1000
    and string.format("[thinking... %s]", duration)
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

local function format_bytes(bytes)
  if bytes < 1024 then return string.format("%d B", bytes) end
  if bytes < 1024 * 1024 then return string.format("%.1f KiB", bytes / 1024) end
  if bytes < 1024 * 1024 * 1024 then return string.format("%.1f MiB", bytes / (1024 * 1024)) end
  return string.format("%.1f GiB", bytes / (1024 * 1024 * 1024))
end

local function loading_widget(state)
  local loading = state.resume_loading
  if loading == nil then return nil end
  local body
  if loading.total == nil then
    body = "[loading session…]"
  else
    local total = math.max(0, loading.total)
    local replayed = math.min(math.max(0, loading.replayed or 0), total)
    local percentage = total > 0 and replayed / total * 100 or 0
    body = string.format(
      "[loading session… bytes %s / %s · %.1f%%]",
      format_bytes(replayed), format_bytes(total), percentage
    )
  end
  return tui.text { content = body, style = STYLE.system, wrap = "none" }
end

local function transcript(state, statusline)
  -- Welcome banner shows on a fresh surface only; the chat widget's
  -- `empty_view` slot accepts a fn returning the banner tree, which
  -- it stacks over an empty scrollable so scroll_position keeps
  -- resolving. Replay-mode opt-out: between sessions.replay.start
  -- and the first replayed conversation projection, the transcript is
  -- briefly empty AND we're rebuilding. Painting the banner here
  -- would flash the welcome copy in the middle of a resume.
  local empty_view
  if state.in_flight == nil and not state.pending and not state.replay_mode then
    empty_view = statusline.welcome_banner
  end
  local concealed = state.resume_loading ~= nil
  return W.chat.view({
    key          = concealed and "transcript-loading" or "transcript",
    entries      = function() return concealed and {} or (state.entries or {}) end,
    render_entry = function(e, i)
      local queued = queued_input.is_queued_entry(state, e)
      return entries_mod.render(e, i, state.expanded_details, queued, state.raw_tool_id)
    end,
    append       = concealed and nil or thinking_widget(state),
    empty_view   = empty_view,
  })
end

-- Keep the engine's render loop alive at ~1Hz while any per-second
-- elapsed counter is on screen — tui.now_ms() only re-evaluates on a
-- render, and the engine renders only on state changes / animation
-- ticks. Without this, the run panel's "Ns" stalls between events.
-- Mount only when something needs to refresh.
local KEEPALIVE_FRAMES = { "", "" }

local function render_keepalive(state)
  -- Toast inclusion is load-bearing: without it the engine renders
  -- only on state changes, so the toast appears once and never
  -- re-renders to run its slide-out / disappearance. duration_ms = 100
  -- keeps the toast slide smooth (~60fps engine tick when active);
  -- Run-panel elapsed counters only need 1Hz but the extra ticks are free.
  local has_toast = state.toasts and #state.toasts > 0
  if not (state.pending or run_panel.any_active(state.runs, tui.now_ms()) or has_toast) then
    return nil
  end
  return tui.animation {
    frames      = KEEPALIVE_FRAMES,
    duration_ms = 100,
  }
end

local function render(state, statusline, slash)
  -- The input drops focus while certain popups own the keyboard. Tool
  -- permission expects single-char A/D; model picker takes printable
  -- chars as filter input — both paths require input to stop
  -- swallowing keys.
  local popup_owns_keys = state.popup and (
    state.popup.variant == "tool_permission" or
    state.popup.variant == "model_picker" or
    state.popup.variant == "session_picker" or
    state.popup.variant == "login_picker" or
    state.popup.variant == "info" or
    state.popup.variant == "warning" or
    state.popup.variant == "error" or
    state.popup.variant == "terminate_workflow" or
    -- Read-only node inspector: prompt structurally inactive while open.
    state.popup.variant == "node_inspector"
  )
  -- Pane focus (Tab/Shift-Tab) gates the prompt the same way popups
  -- do: while the sidebar holds key focus the input drops `focused`,
  -- so the Rust router bubbles every key to the reducer's sidebar
  -- navigation instead of swallowing it into the text field.
  local input_focused = not popup_owns_keys
    and state.focus ~= "sidebar"
    and state.resume_loading == nil
  local input_border_style
  if type(statusline.input_border_style) == "function" then
    input_border_style = statusline.input_border_style(state, input_focused)
  else
    input_border_style = input_focused and STYLE.input_border or STYLE.input_border_unfocused
  end
  -- The prompt widget owns trigger detection + popup rendering + Tab
  -- routing for both slash and @-path completion. Chat.lua declares
  -- the completion sources via slash.completions() and reads the
  -- selected match back from state.completion on submit (Enter
  -- promotes the highlighted slash match to its command text).
  -- When the sidebar owns keys the prompt is dimmed and inactive; its
  -- placeholder states the way back so the greyed state self-explains
  -- instead of reading as broken. Shows only while the draft is empty
  -- (a preserved draft stays visible), which is exactly the confusing
  -- case.
  local prompt_placeholder
  if state.resume_loading ~= nil then
    prompt_placeholder = "— rebuilding session"
  elseif not input_focused and state.focus == "sidebar" then
    prompt_placeholder = "— Tab to return"
  end
  local input_field = W.prompt.view({
    state          = {
      value      = state.input_value,
      completion = state.completion,
    },
    key             = "input",
    focused         = input_focused,
    placeholder     = prompt_placeholder,
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
      loading_widget(state),
      tui.expanded { child = transcript(state, statusline) },
      input_field,
      statusline.view(state),
      blank_row(),
      render_keepalive(state),
    },
  }

  -- Outer row: left column (chat) | separator | sidebar. No outer
  -- padding — the sidebar's vertical separator reaches the full
  -- window height (top and bottom edges flush), and per-element
  -- spacing is handled inside left_column and run_panel.panel.
  local main_row = tui.row {
    gap = 0,
    children = compact {
      tui.expanded { child = left_column },
      state.show_sidebar and run_panel.vertical_separator() or nil,
      state.show_sidebar and run_panel.panel(state)         or nil,
    },
  }

  return tui.stack {
    children = compact {
      main_row,
      popups.help(state),
      popups.message(state),
      popups.model_picker(state),
      popups.session_picker(state),
      popups.login_picker(state),
      popups.tool_permission(state),
      popups.terminate_workflow(state),
      popups.node_inspector(state),
      -- Toast renders last so it sits above input, statusline, and
      -- every popup — non-blocking notifications must never be
      -- occluded by chrome below them.
      W.toast.view({ toasts = state.toasts }),
    },
  }
end

function M.build(deps)
  deps = deps or {}
  assert(type(deps.statusline) == "table", "chat view requires a statusline dependency")
  assert(type(deps.slash) == "table", "chat view requires a slash dependency")
  assert(type(deps.statusline.view) == "function", "chat statusline requires view(state)")
  assert(type(deps.slash.completions) == "function", "chat slash requires completions()")
  return function(state)
    return render(state, deps.statusline, deps.slash)
  end
end

return M
