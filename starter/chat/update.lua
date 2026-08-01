-- Reducer for the chat surface. Receives a message + current state,
-- returns (next_state, effects). Effects are NCP envelopes the engine
-- routes onto the bus. Pure update except for `tui.now_ms` reads and
-- `tui.scroll_*` / `tui.copy_to_clipboard` side-effect bindings.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget

local common        = require("libs.chat.common")
local preview_state = require("libs.chat.preview_state")
local slash        = require("chat.slash")
local sessions     = require("libs.chat.sessions")
local at_path      = require("libs.chat.at_path")
local history      = require("libs.chat.history")
local run_panel    = require("libs.chat.run_panel")
local transcript   = require("libs.chat.transcript")
local popups       = require("libs.chat.popups")
local usage_view   = require("libs.chat.usage")
local Entry        = require("libs.chat.entry")
local log          = require("libs.chat.log")
local tool_display = require("libs.chat.tool_display")
local height_cache = require("libs.chat.height_cache")
local workflow_controls = require("libs.chat.workflow_controls")
local exit_controls = require("libs.chat.exit_controls")
local queued_input = require("libs.chat.queued_input")
local mag_run_bindings = require("libs.mag-run-bindings")

local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL
local format_args   = common.format_args

local M = {}
local run_bindings = mag_run_bindings.new()

local function active_config()
  local ok, cfg = pcall(function() return require("config").active end)
  if ok and type(cfg) == "table" then return cfg end
  return {}
end

local function pop_next_popup(state_tbl)
  local queue = state_tbl.popup_queue
  if type(queue) == "table" and #queue > 0 then
    local next_popup = table.remove(queue, 1)
    if #queue == 0 then queue = NIL_SENTINEL end
    return { popup = next_popup, popup_queue = queue }
  end
  return { popup = NIL_SENTINEL }
end

local function is_permission_popup(popup)
  return type(popup) == "table" and popup.variant == "tool_permission"
end

local function enqueue_permission_popup(state, popup)
  if is_permission_popup(state.popup) then
    local queue = {}
    for _, queued in ipairs(state.popup_queue or {}) do queue[#queue + 1] = queued end
    queue[#queue + 1] = popup
    return shallow_merge(state, { popup_queue = queue })
  end
  return shallow_merge(state, { popup = popup })
end

local function is_mag_approval(popup, run_id, correlation)
  return is_permission_popup(popup)
      and popup.permission_kind == "mag_approval"
      and popup.run_id == run_id
      and (correlation == nil or popup.correlation == correlation)
end

local function retract_mag_approvals(state, run_id, correlation)
  local current_matches = is_mag_approval(state.popup, run_id, correlation)
  local queue = {}
  for _, popup in ipairs(state.popup_queue or {}) do
    if not is_mag_approval(popup, run_id, correlation) then
      queue[#queue + 1] = popup
    end
  end

  local patch = {}
  if current_matches then
    local remaining = shallow_merge(state, {
      popup_queue = #queue > 0 and queue or NIL_SENTINEL,
    })
    patch = pop_next_popup(remaining)
  elseif #queue ~= #(state.popup_queue or {}) then
    patch.popup_queue = #queue > 0 and queue or NIL_SENTINEL
  else
    return state
  end
  return shallow_merge(state, patch)
end

-- Per-model context window sizes reported by the provider's model list.
-- Populated by chat.models.listed events; keyed by model id.
local model_context_windows = {}
local model_reasoning_defaults = {}

local function model_key(provider, model)
  if type(provider) ~= "string" or provider == ""
      or type(model) ~= "string" or model == "" then
    return nil
  end
  return provider .. "\0" .. model
end

-- The prompt widget's `handle()` consumes a fixed set of kinds when a
-- completion is active (key.up/down/tab/escape) and on every value
-- change. Wrap it so the reducer can fold the state patch back into
-- chat.lua's flat state shape.
local function prompt_widget_opts(state)
  return {
    state       = {
      value          = state.input_value,
      completion     = state.completion,
      history_cursor = state.history_cursor,
    },
    on_change   = "input.changed",
    on_submit   = "input.submit",
    completions = slash.completions(),
    history     = function() return state.prompt_history or {} end,
  }
end

local function fold_prompt_patch(state, patch)
  -- The widget's patch keys are { value, completion, history_cursor }
  -- inside `state.<patch>`; translate them into chat.lua's flat fields.
  local out = {}
  if patch.value          ~= nil then out.input_value    = patch.value          end
  if patch.completion     ~= nil then out.completion     = patch.completion     end
  if patch.history_cursor ~= nil then out.history_cursor = patch.history_cursor end
  return shallow_merge(state, out)
end

local function has_pending_plan(state)
  local entries = state.entries or {}
  for i = #entries, 1, -1 do
    local entry = entries[i]
    if type(entry) == "table" and entry.kind == "plan" then
      return entry.status == "pending"
    end
  end
  return false
end

local function plan_key(entry)
  if type(entry) ~= "table" then return nil end
  return entry.plan_id or entry.submitted_at
end

local function pending_plan_status(state, key)
  local pending = state.pending_plan_status
  if type(pending) ~= "table" or key == nil then return nil end
  return pending[key]
end

local function without_pending_plan_status(state, key)
  local pending = state.pending_plan_status
  if type(pending) ~= "table" or key == nil or pending[key] == nil then return state end
  local next_pending = {}
  local any = false
  for k, v in pairs(pending) do
    if k ~= key then
      next_pending[k] = v
      any = true
    end
  end
  return shallow_merge(state, {
    pending_plan_status = any and next_pending or NIL_SENTINEL,
  })
end

-- Pure-update prune for stale panel runs + expired toasts.
local function prune_expired(state)
  local now = tui.now_ms()
  local pruned = run_panel.prune(state.runs or {}, now)
  if pruned ~= state.runs then
    state = shallow_merge(state, { runs = pruned })
    -- Captured agent streams and sidebar fold state follow the run
    -- lifecycle: buffers, scope bindings, and fold entries for pruned
    -- runs go with them.
    state = preview_state.prune(state, pruned)
    state = run_panel.prune_folds(state, pruned)
  end
  local toasts = state.toasts
  if toasts ~= nil and #toasts > 0 then
    local kept = {}
    for _, t in ipairs(toasts) do
      if not W.toast.is_expired(t, now) then kept[#kept + 1] = t end
    end
    if #kept ~= #toasts then
      state = shallow_merge(state, { toasts = kept })
    end
  end
  return state
end

-- ── dispatch handlers ─────────────────────────────────────────────────

-- Collapse the transcript's virtual-scroll bookkeeping back to the empty
-- state. Clearing `state.entries` alone leaves the scrollable pinned to the
-- previous session's extent — the geometry cache keeps the old total height,
-- the height cache keeps per-entry heights, and the scroll offset stays at
-- the old bottom — so an emptied transcript renders a long blank scrolled
-- region until new content forces a recompute. Every reset-shaped path
-- (/new, /clear, /mode default, resume, session-switch) calls this so the
-- viewport collapses immediately. Keyed "transcript" to match the chat
-- widget's scrollable key (chat/view.lua).
local function reset_transcript_scroll()
  height_cache.invalidate_all()
  if type(tui.virtual_scroll_invalidate) == "function" then
    tui.virtual_scroll_invalidate("transcript")
  end
  if type(tui.scroll_to) == "function" then
    tui.scroll_to("transcript", 0)
  end
end

local function close_and_flush_lead_unit(state)
  return transcript.flush_graph_results(transcript.close_lead_unit(state))
end

local function flush_before_transcript_replace(state)
  -- The old session owns these results. Materialize them before replacing the
  -- visible transcript so they can neither be discarded as pending state nor
  -- appear as if they belonged to the selected session's replay.
  return transcript.flush_graph_results(state)
end

local function handle_input_changed(msg, state)
  local result = W.prompt.handle(prompt_widget_opts(state), msg)
  if result and result.state then
    return fold_prompt_patch(state, result.state), {}
  end
  return state, {}
end

local function handle_input_submit(msg, state)
  local text = msg.value or ""
  -- Slash autocomplete open + Enter → run the highlighted match,
  -- regardless of what fragment the user actually typed. Browser-style
  -- combobox semantics: pressing Enter while the dropdown is open
  -- selects the focused option, it doesn't submit the partial query.
  if state.completion and state.completion.trigger == "/" then
    local c = state.completion
    local m = c.matches and c.matches[c.cursor or 1]
    if m then
      text = "/" .. m.name
    end
  end
  if #text == 0 then return state, {} end
  -- Slash dispatch.
  local cmd, args, _has_ws = slash.parse(text)
  if cmd == "quit" or cmd == "exit" then
    return state, { { kind = "exit" } }
  end
  if cmd == "new" or cmd == "clear" then
    state = flush_before_transcript_replace(state)
    reset_transcript_scroll()
    local cleared = shallow_merge(state, {
      entries = {}, in_flight = NIL_SENTINEL,
      pending_assistant_projection = NIL_SENTINEL,
      pending_graph_results = NIL_SENTINEL, input_value = "",
      pending = false, completion = NIL_SENTINEL,
      runs = {}, sidebar_folds = {},
      node_previews = {}, preview_registry = {}, scope_to_run = {},
      turn_started_at = NIL_SENTINEL,
      last_turn_duration_ms = NIL_SENTINEL,
      last_esc_ms = NIL_SENTINEL,
      escape_token = NIL_SENTINEL,
      escape_count = NIL_SENTINEL,
      history_cursor = NIL_SENTINEL,
      popup = NIL_SENTINEL,
      queued_entry_idx = NIL_SENTINEL,
    })
    return cleared, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.interrupt_all" } },
      { kind = "send_to", target = "engine",
        body = { kind = "sessions.new_request" } },
    }
  end
  if cmd == "help" then
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = { variant = "help" },
    }), {}
  end
  if cmd == "usage" then
    local provider = state.provider
    if type(provider) ~= "string" or provider == ""
        or not (state.supports_usage or {})[provider] then
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = {
          variant = "warning",
          title = "/usage",
          body = "The active provider does not expose account usage.",
        },
      }), {}
    end
    local cached = (state.usage or {})[provider]
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant = "info",
        title = "usage",
        body = cached and usage_view.markdown(cached) or "Fetching usage…",
        usage_provider = provider,
      },
    }), {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.usage.requested", provider = provider } },
    }
  end
  if cmd == "safe" or cmd == "auto" or cmd == "yolo" then
    local s = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
    return s, {
      { kind = "send_to", target = "engine",
        body = { kind = "tool-gate.set_mode", mode = cmd } },
    }
  end
  if cmd == "raw" then
    local requested = args
    if type(requested) ~= "string" or requested == "" then
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = { variant = "warning", title = "/raw", body = "Usage: /raw <tool-call-id>" },
      }), {}
    end
    for _, entry in ipairs(state.entries or {}) do
      if entry.kind == "tool_call" and entry.id == requested then
        height_cache.invalidate_all()
        return shallow_merge(state, {
          input_value = "", completion = NIL_SENTINEL,
          expanded_details = true,
          raw_tool_id = state.raw_tool_id == requested and NIL_SENTINEL or requested,
        }), {}
      end
    end
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = { variant = "warning", title = "/raw", body = "No tool call with id `" .. requested .. "`" },
    }), {}
  end
  if cmd == "login" or cmd == "logout" then
    if args and #args > 0 then
      local supports = (state.supports_login or {})[args]
      if not supports then
        return shallow_merge(state, {
          input_value = "", completion = NIL_SENTINEL,
          popup = {
            variant = "warning",
            title   = "/" .. cmd,
            body    = "Provider `" .. args .. "` doesn't support " .. cmd .. ".",
          },
        }), {}
      end
      local body = { kind = "chat." .. cmd .. "_requested", provider = args }
      return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine", body = body },
      }
    end
    local supports = state.supports_login or {}
    local providers = {}
    for n, st in pairs(state.auth or {}) do
      if supports[n] then
        if cmd == "logout" then
          if st == "connected" then
            providers[#providers + 1] = { name = n, state = st }
          end
        else
          providers[#providers + 1] = { name = n, state = st }
        end
      end
    end
    table.sort(providers, function(a, b) return a.name < b.name end)
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant   = "login_picker",
        mode      = cmd,
        providers = providers,
        cursor    = 1,
      },
    }), {}
  end
  if cmd == "model" then
    if args and #args > 0 then
      local provider = nil
      local connected = {}
      for n, st in pairs(state.auth or {}) do
        if st == "connected" then connected[#connected + 1] = n end
      end
      table.sort(connected)
      provider = connected[1]
      local body = { kind = "chat.model.set", model = args }
      if provider then body.provider = provider end
      return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine", body = body },
      }
    end
    local providers = {}
    for n, st in pairs(state.auth or {}) do
      providers[#providers + 1] = { name = n, state = st, models = {} }
    end
    table.sort(providers, function(a, b) return a.name < b.name end)
    local awaiting = {}
    for _, prov in ipairs(providers) do awaiting[prov.name] = true end
    local effects = {}
    for _, prov in ipairs(providers) do
      effects[#effects + 1] = {
        kind = "send_to", target = "engine",
        body = { kind = "chat.model.list_requested", provider = prov.name },
      }
    end
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant   = "model_picker",
        providers = providers,
        query     = "",
        cursor    = 1,
        awaiting  = awaiting,
      },
    }), effects
  end
  if cmd == "mode" then
    local mode_arg = (args or ""):lower()
    if mode_arg == "" then
      local result = W.prompt.handle(prompt_widget_opts(state), {
        kind = "input.changed",
        value = "/mode ",
      })
      if result and result.state then
        return fold_prompt_patch(state, result.state), {}
      end
      return shallow_merge(state, {
        input_value = "/mode ",
        completion = NIL_SENTINEL,
        popup = NIL_SENTINEL,
      }), {}
    end
    if mode_arg == "default" or mode_arg == "normal" then
      local cfg = active_config()
      local provider = cfg.default_provider or state.provider
      local model = cfg.default_model or state.model
      local toasts = {}
      for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
      toasts[#toasts + 1] = {
        id = "mode-default-" .. tostring(tui.now_ms()),
        text = "new default session: " .. tostring(provider or "?") .. "/" .. tostring(model or "?"),
        level = "info",
        started_at_ms = tui.now_ms(),
        ttl_ms = 3000,
      }
      state = flush_before_transcript_replace(state)
      reset_transcript_scroll()
      local cleared = shallow_merge(state, {
        entries = {}, in_flight = NIL_SENTINEL,
        pending_assistant_projection = NIL_SENTINEL,
        pending_graph_results = NIL_SENTINEL, input_value = "",
        pending = false, completion = NIL_SENTINEL,
        runs = {}, sidebar_folds = {},
        node_previews = {}, preview_registry = {}, scope_to_run = {},
        turn_started_at = NIL_SENTINEL,
        last_turn_duration_ms = NIL_SENTINEL,
        last_esc_ms = NIL_SENTINEL,
        escape_token = NIL_SENTINEL,
        escape_count = NIL_SENTINEL,
        history_cursor = NIL_SENTINEL,
        popup = NIL_SENTINEL,
        queued_entry_idx = NIL_SENTINEL,
        mode = "default",
        provider = provider,
        model = model,
        toasts = toasts,
      })
      local effects = {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt_all" } },
        { kind = "send_to", target = "engine",
          body = { kind = "sessions.new_request" } },
      }
      if type(provider) == "string" and provider ~= ""
          and type(model) == "string" and model ~= "" then
        effects[#effects + 1] = {
          kind = "send_to", target = "engine",
          body = {
            kind = "chat.model.set",
            provider = provider,
            model = model,
          },
        }
      end
      return cleared, effects
    end
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant = "warning",
        title = "/mode",
        body = "Usage: /mode default",
      },
    }), {}
  end
  if cmd == "think" or cmd == "effort" then
    if args == nil or #args == 0 then
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = {
          variant = "warning",
          title   = "/think",
          body    = "Usage: /think low|medium|high|xhigh",
        },
      }), {}
    end
    local body = { kind = "chat.reasoning.set", effort = args }
    if type(state.provider) == "string" and #state.provider > 0 then
      body.provider = state.provider
    end
    return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
      { kind = "send_to", target = "engine", body = body },
    }
  end
  if cmd == "compact" then
    local body = {
      kind     = "chat.compaction.request",
      trigger  = "manual",
      provider = state.provider,
    }
    local next_state = transcript.push_entry(
      shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }),
      Entry.compaction({
        provider = state.provider,
        model = state.model,
        trigger = "manual",
        status = "pending",
      })
    )
    return next_state, {
      { kind = "send_to", target = "engine", body = body },
    }
  end
  if cmd == "resume" then
    if args and #args > 0 then
      local id = args:match("^([%w%-]+)") or args
      state = flush_before_transcript_replace(state)
      reset_transcript_scroll()
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        entries = {}, in_flight = NIL_SENTINEL,
        pending_assistant_projection = NIL_SENTINEL,
        pending_graph_results = NIL_SENTINEL,
        pending = false, runs = {}, sidebar_folds = {},
        node_previews = {}, preview_registry = {}, scope_to_run = {},
        turn_started_at = NIL_SENTINEL,
        last_turn_duration_ms = NIL_SENTINEL,
        queued_entry_idx = NIL_SENTINEL,
        resume_loading = { session_id = id, replayed = 0 },
        replay_mode = true,
      }), {
        sessions.emit_resume_request(id),
      }
    end
    local rows = sessions.list_recent()
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant  = "session_picker",
        sessions = rows,
        cursor   = 1,
      },
    }), {}
  end
  if has_pending_plan(state) then
    local hist = { text }
    for i, v in ipairs(state.prompt_history or {}) do
      if i >= history.INPUT_HISTORY_MAX then break end
      hist[#hist + 1] = v
    end
    history.persist(hist)
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      prompt_history = hist, history_cursor = NIL_SENTINEL,
    }), {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.review.respond", text = text } },
    }
  end
  if cmd ~= nil then
    -- Unknown slash → generic chat.command for user-defined Lua handlers.
    return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.command", name = cmd, args = args or "" } },
    }
  end
  -- Plain text submit.
  local wire_text = at_path.expand(text)
  local hist = { text }
  for i, v in ipairs(state.prompt_history or {}) do
    if i >= history.INPUT_HISTORY_MAX then break end
    hist[#hist + 1] = v
  end
  history.persist(hist)

  -- When a turn is already in flight, coalesce into a single queued
  -- entry instead of pushing a new user bubble per message.
  if state.pending or state.in_flight ~= nil then
    local next_state = shallow_merge(queued_input.submit(state, wire_text, true), {
      input_value = "", completion = NIL_SENTINEL,
      prompt_history = hist, history_cursor = NIL_SENTINEL,
    })
    tui.scroll_into_view("transcript")
    return next_state, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = wire_text } },
    }
  end

  local cleared = shallow_merge(queued_input.submit(state, wire_text, false), {
    input_value = "", pending = true,
    turn_started_at = tui.now_ms(), completion = NIL_SENTINEL,
    prompt_history = hist,
    history_cursor = NIL_SENTINEL,
  })
  tui.scroll_into_view("transcript")
  return cleared, {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.input.submit", text = wire_text } },
  }
end

local function exit_effects(state)
  return state, {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.interrupt_all" } },
    { kind = "exit" },
  }
end

local function handle_ctrl_c(_msg, state)
  local next_state, decisions = exit_controls.press(state, tui.now_ms())
  local effects = {}
  for _, decision in ipairs(decisions) do
    if decision.kind == "schedule_exit_timeout" then
      effects[#effects + 1] = {
        kind = "dispatch_after", delay_ms = decision.delay_ms,
        body = { kind = "chat.exit_timeout", token = decision.token },
      }
    elseif decision.kind == "exit" then
      return exit_effects(next_state)
    end
  end
  return next_state, effects
end

local function handle_exit_timeout(msg, state)
  return exit_controls.timeout(state, msg.token), {}
end

local function handle_toggle_sidebar(_msg, state)
  local showing = not state.show_sidebar
  local patch = { show_sidebar = showing }
  -- Hiding a focused sidebar would strand key focus on an invisible
  -- pane; hand it back to the prompt.
  if not showing and state.focus == "sidebar" then
    patch.focus = "prompt"
  end
  return shallow_merge(state, patch), {}
end

-- Tab / Shift-Tab cycle key focus between the prompt and the sidebar.
-- Two panes ⇒ both keys toggle; the pair future-proofs for >2. The
-- completion popup wins Tab (existing prompt-widget navigation), and
-- any open popup owns the keyboard, so both cases leave focus alone.
local function handle_focus_cycle(msg, state)
  if state.completion ~= nil then
    local result = W.prompt.handle(prompt_widget_opts(state), msg)
    if result ~= nil then
      return fold_prompt_patch(state, result.state or {}), {}
    end
    return state, {}
  end
  if state.popup ~= nil then return state, {} end
  if state.focus == "sidebar" then
    return shallow_merge(state, { focus = "prompt" }), {}
  end
  if not state.show_sidebar then return state, {} end
  -- Empty sidebar refuses focus unless it exposes the bounded completed-run
  -- inspection target. That target has no ordinary row, but focusing the pane
  -- is still how its visible Space hint becomes actionable.
  local now = tui.now_ms()
  if #run_panel.row_model(state, now) == 0
      and run_panel.recent_completed(state.runs, now) == nil then
    local toasts = {}
    for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
    toasts[#toasts + 1] = {
      id            = "empty-sidebar-" .. tostring(tui.now_ms()),
      text          = "can't focus an empty sidebar",
      level         = "warn",
      started_at_ms = tui.now_ms(),
      ttl_ms        = 2500,
    }
    return shallow_merge(state, { toasts = toasts }), {}
  end
  return shallow_merge(state, {
    focus          = "sidebar",
    sidebar_cursor = state.sidebar_cursor or 1,
  }), {}
end

local function handle_toggle_expand(_msg, state)
  if type(tui.virtual_scroll_invalidate) == "function" then
    tui.virtual_scroll_invalidate("chat")
  end
  height_cache.invalidate_all()
  local expanded = not state.expanded_details
  return shallow_merge(state, {
    expanded_details = expanded,
    raw_tool_id = expanded and state.raw_tool_id or NIL_SENTINEL,
  }), {}
end

local function handle_toggle_tool_raw(_msg, state)
  if not state.expanded_details then return state, {} end
  local entries = state.entries or {}
  for i = #entries, 1, -1 do
    local entry = entries[i]
    if entry.kind == "tool_call" then
      height_cache.invalidate_all()
      local raw_tool_id = state.raw_tool_id == entry.id and NIL_SENTINEL or entry.id
      return shallow_merge(state, { raw_tool_id = raw_tool_id }), {}
    end
  end
  return state, {}
end

local function handle_help_key(_msg, state)
  if state.input_value == "" then
    return shallow_merge(state, { popup = { variant = "help" } }), {}
  end
  return state, {}
end

local function apply_control_decisions(state, decisions, metadata)
  local effects = {}
  if metadata and metadata.restored_queue then height_cache.invalidate_all() end
  for _, decision in ipairs(decisions) do
    if decision.kind == "schedule_escape_timeout" then
      effects[#effects + 1] = {
        kind = "dispatch_after", delay_ms = decision.delay_ms,
        body = { kind = "chat.escape_timeout", token = decision.token },
      }
    elseif decision.kind == "steer_queued" then
      effects[#effects + 1] = {
        kind = "send_to", target = "engine", body = { kind = "chat.steer" },
      }
    elseif decision.kind == "hard_stop_lead" then
      state = close_and_flush_lead_unit(state)
      effects[#effects + 1] = {
        kind = "send_to", target = "engine",
        body = { kind = "chat.interrupt", drop_queued = true },
      }
    elseif decision.kind == "terminate_all_workflows" then
      effects[#effects + 1] = {
        kind = "send_to", target = "engine",
        body = { kind = "chat.workflows.terminate_requested", scope = "all" },
      }
      effects[#effects + 1] = {
        kind = "send_to", target = "mag", body = { kind = "mag.kill_all_runs" },
      }
    elseif decision.kind == "terminate_workflow" then
      effects[#effects + 1] = {
        kind = "send_to", target = "engine",
        body = {
          kind = "chat.workflows.terminate_requested",
          scope = "one",
          run_id = decision.run_id,
        },
      }
      effects[#effects + 1] = {
        kind = "send_to", target = "mag",
        body = { kind = "mag.kill_run", run_id = decision.run_id },
      }
    end
  end
  return state, effects
end

local function handle_escape_timeout(msg, state)
  return apply_control_decisions(workflow_controls.escape_timeout(state, msg.token))
end

local function permission_response(popup, approved)
  if popup.permission_kind == "mag_approval" then
    local reply = { kind = "mag.ApprovalReply", approved = approved }
    if not approved then reply.reason = "Denied by user" end
    return {
      kind = "send_to", target = "mag",
      body = {
        kind = "mag.apply",
        run_id = popup.run_id,
        source = "chat.human_approval",
        modification = { messages = { { to = popup.gate_id, content = reply } } },
      },
    }
  end
  return {
    kind = "send_to", target = "engine",
    body = {
      kind = "tool.permission_response",
      id = popup.id,
      decision = approved and "approve" or "deny",
    },
  }
end

local function resolve_permission(state, approved)
  local popup = state.popup
  return shallow_merge(state, pop_next_popup(state)), {
    permission_response(popup, approved),
  }
end

local function handle_escape(_msg, state)
  -- 1a) Info / warning / error popups: Esc dismisses the popup only
  -- (toasts stay). Matches the same Enter/Q path in route_keys_and_popups.
  if state.popup
     and (state.popup.variant == "info"
       or state.popup.variant == "warning"
       or state.popup.variant == "error") then
    return shallow_merge(state, { popup = NIL_SENTINEL }), {}
  end
  -- 1b) close popup or toasts
  local has_toast = state.toasts and #state.toasts > 0
  if state.popup or has_toast then
    -- Tool permission ESC = deny.
    if state.popup and state.popup.variant == "tool_permission" then
      return resolve_permission(state, false)
    end
    return shallow_merge(state, { popup = NIL_SENTINEL, toasts = {} }), {}
  end
  -- 1c) sidebar focused → hand focus back to the prompt. Sits before
  -- the interrupt path deliberately: Esc while navigating the sidebar
  -- is "leave the pane", never "cancel the turn".
  if state.focus == "sidebar" then
    return shallow_merge(state, { focus = "prompt" }), {}
  end
  -- 2) close completion dropdown (slash or @-path)
  if state.completion ~= nil then
    return shallow_merge(state, { completion = NIL_SENTINEL }), {}
  end
  -- 3) cancel prompt-history navigation (clear recalled value)
  if state.history_cursor ~= nil then
    return shallow_merge(state, {
      input_value    = "",
      history_cursor = NIL_SENTINEL,
    }), {}
  end
  -- 4) The reusable control machine resolves the tokenized Esc gesture.
  return apply_control_decisions(workflow_controls.escape(state, tui.now_ms()))
end

-- ── session lifecycle ─────────────────────────────────────────────────

local function handle_session_end(_msg, state)
  run_bindings = mag_run_bindings.new()
  state = close_and_flush_lead_unit(state)
  return shallow_merge(state, {
    in_flight        = NIL_SENTINEL,
    pending_assistant_projection = NIL_SENTINEL,
    pending          = false,
    turn_started_at  = NIL_SENTINEL,
    last_turn_duration_ms = NIL_SENTINEL,
    popup            = NIL_SENTINEL,
    popup_queue      = NIL_SENTINEL,
    toasts           = {},
    completion       = NIL_SENTINEL,
    last_esc_ms      = NIL_SENTINEL,
    escape_token     = NIL_SENTINEL,
    escape_count     = NIL_SENTINEL,
    runs             = {},
    node_previews    = {}, preview_registry = {},
    scope_to_run     = {},
    sidebar_folds    = {},
    lead_chat_id     = NIL_SENTINEL,
    lead_chat_prefix = NIL_SENTINEL,
    instruction_notice_ids = {},
  }), {}
end

local function handle_session_start(msg, state)
  run_bindings = mag_run_bindings.new()
  local runs = state.runs
  if os.getenv("NEFOR_TEST_SIDEBAR_OVERFLOW") == "1" and next(runs or {}) == nil then
    local nodes = {}
    for i = 1, 20 do
      nodes[string.format("fixture-%02d.member", i)] = {
        reasoner = "test-fixture", status = "pending", started_at_ms = 0,
        finished_at_ms = nil, seq = i,
      }
    end
    runs = {
      ["sidebar-overflow-fixture"] = {
        run_id = "sidebar-overflow-fixture", run_name = "sidebar-overflow-fixture",
        principal = "test", total_nodes = 20, started_at_ms = 0, nodes = nodes,
        completed_at_ms = nil, status = nil, rejected = 0, noops = 0, actor_seq = 20,
      },
    }
  elseif os.getenv("NEFOR_TEST_SIDEBAR_OVERFLOW") ~= "1" then
    runs = {}
  end
  return shallow_merge(state, {
    session_id = msg.session_id,
    runs = runs,
    node_previews = {}, preview_registry = {}, scope_to_run = {},
    lead_chat_id = NIL_SENTINEL, lead_chat_prefix = NIL_SENTINEL,
    instruction_notice_ids = {},
  }), {}
end

local function handle_resume_loading(msg, state)
  return shallow_merge(state, {
    resume_loading = { session_id = msg.session_id, replayed = 0 },
    replay_mode = true,
  }), {}
end

local function handle_replay_start(msg, state)
  if state.resume_loading == nil then
    return shallow_merge(state, { replay_mode = true }), {}
  end
  local loading = state.resume_loading
  return shallow_merge(state, {
    replay_mode = true,
    resume_loading = {
      session_id = loading.session_id or msg.session_id,
      replayed = loading.replayed or 0,
      total = msg.count or 0,
    },
  }), {}
end

local function handle_replay_progress(msg, state)
  if state.resume_loading == nil then return state, {} end
  local previous = state.resume_loading.replayed or 0
  local replayed = math.max(previous, msg.replayed or 0)
  return shallow_merge(state, {
    resume_loading = {
      session_id = state.resume_loading.session_id or msg.session_id,
      replayed = replayed,
      total = msg.total or state.resume_loading.total,
    },
  }), {}
end

local function handle_replay_end(_msg, state)
  if state.resume_loading == nil then
    return shallow_merge(state, { replay_mode = NIL_SENTINEL }), {}
  end
  return state, {}
end

local function handle_resume_done(_msg, state)
  return shallow_merge(state, {
    replay_mode = NIL_SENTINEL,
    resume_loading = NIL_SENTINEL,
  }), {}
end

local function handle_chat_reset(_msg, state)
  return close_and_flush_lead_unit(state), {}
end

-- ── inbound chat-contract events ──────────────────────────────────────

local function handle_instruction_notice(msg, state)
  local invocation = msg.invocation
  local notice_id = msg.notice_id
  local valid_provenance = type(invocation) == "table"
      and type(invocation.session_id) == "string" and invocation.session_id ~= ""
      and type(invocation.run_id) == "string" and invocation.run_id ~= ""
      and type(invocation.run_scope) == "string" and invocation.run_scope ~= ""
      and type(invocation.actor_id) == "string" and invocation.actor_id ~= ""
      and type(invocation.capability_id) == "string" and invocation.capability_id ~= ""
      and invocation.capability_id:sub(1, #invocation.run_scope + 1)
        == invocation.run_scope .. "/"
  if msg._event_source ~= "engine"
      or not valid_provenance
      or not run_bindings.validate(invocation, state.session_id)
      or type(notice_id) ~= "string" or notice_id == ""
      or type(msg.path) ~= "string" or msg.path == ""
      or type(msg.text) ~= "string" or msg.text == "" then
    return state, {}
  end
  if invocation.principal == "subagent" then
    -- Instruction notices remain visible in the lead transcript only; actor
    -- previews are populated by their declared telemetry and provider traffic.
    return state, {}
  end
  if invocation.principal ~= "lead" then
    return state, {}
  end
  if (state.scope_to_run or {})[invocation.run_scope] ~= invocation.run_id then
    return state, {}
  end
  local seen = state.instruction_notice_ids or {}
  if seen[notice_id] then return state, {} end
  local next_seen = {}
  for id, value in pairs(seen) do next_seen[id] = value end
  next_seen[notice_id] = true
  local next_state = shallow_merge(state, { instruction_notice_ids = next_seen })
  return transcript.push_entry(next_state,
    Entry.agents_md(msg.path, msg.dir or msg.path, msg.text, notice_id)), {}
end

local function handle_message_append(msg, state)
  local text = msg.text or ""
  if #text == 0 then return state, {} end
  local role = msg.role or "system"
  -- The pending structured answer owns the provider round's chronological
  -- position. Same-turn transcript appends can arrive before its durable
  -- assistant projection; only an explicit terminal failure closes that ownership.
  if role == "system" and (text:match("^Error:")
      or text:match("^%[lead turn failed%]")) then
    state = transcript.close_assistant_projection(state)
  end
  if role == "user" then
    local reconciled, matched = queued_input.reconcile_echo(state, text)
    if matched then return reconciled, {} end
  end
  if role == "assistant" then
    local next_state, handled = transcript.reduce_assistant_event(state, msg)
    if handled then
      return transcript.flush_graph_results_if_stable(next_state), {}
    end
  end
  local turn_state = role == "system"
    and { pending = false, turn_started_at = NIL_SENTINEL }
    or  {}

  -- Legacy instruction-shaped chat messages fail closed. Dedicated
  -- chat.instruction.notice events are the only accepted projection path;
  -- metadata is irrelevant because old sessions contain absent, malformed,
  -- and contradictory path/dir combinations.
  if role == "system" and (text:match("^Local instruction files available")
        or text:match("^%[Loaded .- because tool call touched a file")) then
    return shallow_merge(state, turn_state), {}
  end

  if role == "system" then
    return transcript.push_entry(shallow_merge(state, turn_state),
      Entry.system(text)
    ), {}
  end
  return transcript.push_entry(shallow_merge(state, turn_state), {
    role = role, text = text, kind = "text",
  }), {}
end

local function handle_error_append(msg, state)
  local title = type(msg.title) == "string" and msg.title or "Something went wrong"
  local message = type(msg.message) == "string" and msg.message or "The operation failed."
  local next_state = transcript.close_assistant_projection(state)
  next_state = shallow_merge(next_state, {
    pending = false,
    turn_started_at = NIL_SENTINEL,
  })
  next_state = transcript.push_entry(next_state,
    Entry.error(title, message, msg.retryable))
  return transcript.flush_graph_results(next_state), {}
end

-- The transcript renders exactly one conversation: the lead's. Its
-- binding arrives via `chat.lead.bound` (broadcast by the agentic-loop,
-- and replayed from the session log on /resume) in one of two forms:
--
--   * `{ chat_prefix }` — prefix-match; the lead's kernel turn-program.
--     Kernel chat handles are run-scoped and per-round
--     (`r<K>/<actor>@r<seq>` — a new id every round), so the spawner
--     binds the `r<K>/<actor>@` prefix once per run.
--   * `{ chat_id }` — exact-match; a single long-lived provider chat
--     (retained for replayed logs of older sessions).
--
-- Foreign chats (dispatched kernel runs' actor chats, other scopes)
-- stream on the same bus deliberately — the session log and run-panel
-- consumers want the deltas — but stay out of the transcript. Events
-- without a chat_id, or arriving before any binding is known, stay
-- renderable (mock providers and pre-binding turns).
local function is_foreign_chat(msg, state)
  local cid = msg.chat_id
  if type(cid) ~= "string" or #cid == 0 then return false end
  local prefix = state.lead_chat_prefix
  if type(prefix) == "string" and #prefix > 0 then
    return cid:sub(1, #prefix) ~= prefix
  end
  local lead = state.lead_chat_id
  if type(lead) ~= "string" or #lead == 0 then return false end
  return cid ~= lead
end

-- Not gated on replay_mode: replay must rebuild the binding so
-- replayed foreign deltas stay out of the transcript too. A binding in
-- either form supersedes the other (the newest broadcast wins — one
-- lead conversation at a time).
local function handle_lead_chat_bound(msg, state)
  local prefix = msg.chat_prefix
  if type(prefix) == "string" and #prefix > 0 then
    if state.lead_chat_prefix == prefix then return state, {} end
    return shallow_merge(state, {
      lead_chat_prefix = prefix,
      lead_chat_id     = NIL_SENTINEL,
    }), {}
  end
  local cid = msg.chat_id
  if type(cid) ~= "string" or #cid == 0 then return state, {} end
  if state.lead_chat_id == cid and state.lead_chat_prefix == nil then
    return state, {}
  end
  return shallow_merge(state, {
    lead_chat_id     = cid,
    lead_chat_prefix = NIL_SENTINEL,
  }), {}
end

local function handle_stream_delta(msg, state)
  if is_foreign_chat(msg, state) then return state, {} end
  local t = msg.text or msg.delta or ""
  if #t == 0 then return state, {} end
  return transcript.append_assistant_delta(state, t), {}
end

local function handle_stream_end(msg, state)
  if is_foreign_chat(msg, state) then return state, {} end
  local next_state = transcript.reduce_assistant_event(state, msg)
  return transcript.flush_graph_results_if_stable(next_state), {}
end

local function handle_reasoning_delta(msg, state)
  if is_foreign_chat(msg, state) then return state, {} end
  local t = msg.text or msg.delta or ""
  if #t == 0 then return state, {} end
  return transcript.append_reasoning_delta(state, t), {}
end

local function handle_reasoning_end(msg, state)
  if is_foreign_chat(msg, state) then return state, {} end
  return transcript.finalize_reasoning(state, msg.duration_ms), {}
end

local function handle_session_stats(msg, state)
  if is_foreign_chat(msg, state) then return state, {} end
  local next_state = transcript.reduce_assistant_event(state, msg)
  local stats = shallow_merge(next_state.stats or {}, {})
  for k, v in pairs(msg) do
    if k ~= "kind" then stats[k] = v end
  end
  local current_context_tokens = msg.last_turn_context_tokens
    or msg.last_turn_input_tokens
    or msg.context_tokens
    or msg.prompt_tokens
  local patch = { stats = stats }
  if current_context_tokens ~= nil then
    patch.current_context_tokens = current_context_tokens
  end
  local s = shallow_merge(next_state, patch)
  if msg.model then
    local mt = msg.max_context_tokens
      or model_context_windows[msg.model]
      or state.max_tokens
    s = shallow_merge(s, { model = msg.model, max_tokens = mt })
  end
  return s, {}
end

local function handle_usage_updated(msg, state)
  local provider = msg.provider or ""
  if provider == "" then return state, {} end
  local function merge_usage_table(base, incoming)
    local merged = shallow_merge(type(base) == "table" and base or {}, {})
    for key, value in pairs(incoming or {}) do
      if type(value) == "table" and type(merged[key]) == "table" then
        merged[key] = merge_usage_table(merged[key], value)
      else
        merged[key] = value
      end
    end
    return merged
  end
  local usage = {}
  for k, v in pairs(state.usage or {}) do usage[k] = v end
  local previous = usage[provider] or {}
  local snapshot = merge_usage_table(previous, msg)
  snapshot.kind = nil
  snapshot.provider = nil
  usage[provider] = snapshot
  local popup = state.popup
  if popup and popup.usage_provider == provider then
    popup = {
      variant = "info",
      title = "usage",
      body = usage_view.markdown(snapshot),
      usage_provider = provider,
    }
  end
  return shallow_merge(state, { usage = usage, popup = popup }), {}
end

local function handle_usage_error(msg, state)
  local provider = msg.provider or ""
  if not (state.popup and state.popup.usage_provider == provider) then
    return state, {}
  end
  return shallow_merge(state, {
    popup = {
      variant = "error",
      title = "usage",
      body = msg.message or "Could not refresh usage.",
    },
  }), {}
end

local function handle_tool_start(msg, state)
  local input_str
  if type(msg.input) == "string" then input_str = msg.input
  elseif type(msg.input) == "table" then input_str = "(object)"
  else input_str = "" end
  local raw_input = msg.input
  local contract = (state.tool_displays or {})[msg.name]
  state = transcript.close_assistant_projection(state)
  return transcript.push_entry(state, Entry.tool_call(
    msg.id or "", msg.name or "?", input_str,
    type(msg.input) == "table" and msg.input or nil,
    contract, raw_input)), {}
end

local function handle_tool_register(msg, state)
  -- Session history may contain an older persisted catalog. A live gate
  -- registration already established in this process is authoritative;
  -- replay only rebuilds the catalog on cold start / late attachment.
  if state.replay_mode and state.tool_displays_live then return state, {} end
  local displays = {}
  for _, spec in ipairs(msg.tools or {}) do
    if type(spec.name) ~= "string" or spec.name == "" then
      error("tool.register: tool name must be a non-empty string")
    end
    local ok, err = tool_display.validate(spec.display)
    if not ok then error("tool.register " .. spec.name .. ": " .. tostring(err)) end
    displays[spec.name] = spec.display
  end
  local patch = { tool_displays = displays }
  if not state.replay_mode then patch.tool_displays_live = true end
  return shallow_merge(state, patch), {}
end

local function handle_tool_end(msg, state)
  local next_state = transcript.attach_tool_end(
    state, msg.id or "", msg.output or "", msg.error == true)
  return transcript.flush_graph_results_if_stable(next_state), {}
end

local function handle_graph_result_append(msg, state)
  return transcript.append_graph_result(state,
    Entry.graph_result(
      msg.run_id or "",
      msg.status or "success",
      msg.nodes or {},
      msg.output,
      msg.error,
      msg.duration_ms,
      msg.run_name)
  ), {}
end

local function handle_plan_append(msg, state)
  local text = msg.text or ""
  if #text == 0 then return state, {} end
  local key = msg.plan_id or msg.submitted_at
  local submitted_at = msg.submitted_at
  if key ~= nil then
    for _, v in ipairs(state.entries) do
      if v.kind == "plan" and plan_key(v) == key then
        return state, {}
      end
    end
  end
  local approved = msg.approved
  local pending_status = pending_plan_status(state, key)
  local status
  if approved == true or pending_status == true then
    status = "approved"
  elseif approved == false or pending_status == false then
    status = "rejected"
  end
  local next_state = transcript.push_entry(state,
    Entry.plan(text, submitted_at, msg.plan_id, status)
  )
  return without_pending_plan_status(next_state, key), {}
end

local function handle_compaction_commit(msg, state)
  if type(msg.model_context_artifact) ~= "table" then return state, {} end
  local pending_idx
  for i = #state.entries, 1, -1 do
    local e = state.entries[i]
    if type(e) == "table" and e.kind == "compaction" and e.status == "pending" then
      pending_idx = i
      if (e.provider ~= nil and msg.provider ~= e.provider)
          or (e.model ~= nil and msg.model ~= e.model) then
        return state, {}
      end
      break
    end
  end

  local committed = Entry.compaction({
    chat_id = msg.chat_id,
    provider = msg.provider,
    model = msg.model,
    strategy = msg.strategy,
    trigger = msg.trigger,
    display_summary = msg.display_summary,
    model_context_artifact = msg.model_context_artifact,
    metadata = msg.metadata,
  })
  local entries = {}
  for i = 1, #state.entries do entries[i] = state.entries[i] end
  if pending_idx ~= nil then entries[pending_idx] = committed end
  local next_state
  if pending_idx ~= nil then
    next_state = shallow_merge(state, { entries = entries })
  else
    next_state = transcript.push_entry(state, committed)
  end
  return shallow_merge(next_state, { current_context_tokens = NIL_SENTINEL }), {}
end

local function handle_compaction_failed(msg, state)
  local failed = Entry.compaction({
    provider = msg.provider,
    model = msg.model,
    trigger = msg.trigger,
    status = "failed",
    display_summary = msg.message or "Context compaction failed.",
  })
  local entries = {}
  local replaced = false
  for i = 1, #state.entries do entries[i] = state.entries[i] end
  for i = #entries, 1, -1 do
    local e = entries[i]
    if type(e) == "table" and e.kind == "compaction" and e.status == "pending" then
      entries[i] = failed
      replaced = true
      break
    end
  end
  if replaced then
    return shallow_merge(state, { entries = entries }), {}
  end
  return transcript.push_entry(state, failed), {}
end

local function handle_plan_approved(msg, state)
  local approved = (msg.approved == true)
  local key = msg.plan_id or msg.submitted_at
  local entries = {}
  local target_idx
  for i, v in ipairs(state.entries) do
    if v.kind == "plan" and v.status == "pending" then
      if key == nil or plan_key(v) == key then
        target_idx = i
      end
    end
    entries[i] = v
  end
  if target_idx == nil then
    if key == nil then return state, {} end
    local pending = {}
    for k, v in pairs(state.pending_plan_status or {}) do pending[k] = v end
    pending[key] = approved
    return shallow_merge(state, { pending_plan_status = pending }), {}
  end
  entries[target_idx] = shallow_merge(entries[target_idx], {
    status = approved and "approved" or "rejected",
  })
  return without_pending_plan_status(shallow_merge(state, { entries = entries }), key), {}
end

local function handle_popup(msg, state)
  if state.replay_mode then return state, {} end
  local v = msg.level or "info"
  return shallow_merge(state, {
    popup = {
      variant = v,
      title   = msg.title or v,
      body    = msg.message or msg.text or "",
      source  = msg.source,
    },
  }), {}
end

local function handle_toast(msg, state)
  if state.replay_mode then return state, {} end
  local now = tui.now_ms()
  local ttl = msg.ttl_ms or 2000
  local toasts = {}
  for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
  toasts[#toasts + 1] = {
    id            = msg.id or tostring(now) .. "-" .. tostring(#toasts + 1),
    text          = msg.text or "",
    level         = msg.level or "info",
    started_at_ms = now,
    ttl_ms        = ttl,
  }
  return shallow_merge(state, { toasts = toasts }), {}
end

local function handle_model_set_ack(msg, state)
  if state.replay_mode then return state, {} end
  if type(msg.provider) == "string" and msg.provider ~= state.provider then
    return state, {}
  end
  local provider = msg.provider or state.provider
  local effort = msg.reasoning_effort or state.reasoning_effort
  local k = model_key(provider, msg.model)
  if effort == nil and k ~= nil then
    effort = model_reasoning_defaults[k]
  end
  return shallow_merge(state, {
    model = msg.model or state.model,
    provider = provider,
    reasoning_effort = effort,
    max_tokens = model_context_windows[msg.model] or state.max_tokens,
  }), {}
end

local function handle_reasoning_set_ack(msg, state)
  if state.replay_mode then return state, {} end
  return shallow_merge(state, {
    provider = msg.provider or state.provider,
    reasoning_effort = msg.effort or msg.reasoning_effort or state.reasoning_effort,
  }), {}
end

local function handle_models_listed(msg, state)
  -- Absorb per-model context windows if the provider reported them.
  local active_ctx = nil
  if type(msg.context_windows) == "table" then
    for model_id, ctx_size in pairs(msg.context_windows) do
      if type(ctx_size) == "number" and ctx_size > 0 then
        model_context_windows[model_id] = ctx_size
        if model_id == state.model then active_ctx = ctx_size end
      end
    end
  end
  local active_effort = nil
  if type(msg.model_capabilities) == "table" then
    local provider = msg.provider or ""
    for model_id, caps in pairs(msg.model_capabilities) do
      local reasoning = type(caps) == "table" and caps.reasoning or nil
      local default = type(reasoning) == "table" and reasoning.default or nil
      local k = model_key(provider, model_id)
      if k ~= nil and type(default) == "string" and #default > 0 then
        model_reasoning_defaults[k] = default
        if provider == state.provider and model_id == state.model then
          active_effort = default
        end
      end
    end
  end
  local next_state = state
  if active_ctx ~= nil or (active_effort ~= nil and state.reasoning_effort == nil) then
    next_state = shallow_merge(state, {
      max_tokens       = active_ctx or state.max_tokens,
      reasoning_effort = state.reasoning_effort or active_effort,
    })
  end
  -- Update the open model_picker popup if one is up; otherwise drop.
  if not (next_state.popup and next_state.popup.variant == "model_picker") then
    return next_state, {}
  end
  local provider = msg.provider or ""
  local list = msg.models or {}
  local models = {}
  if type(list) == "table" then
    for _, m in ipairs(list) do models[#models + 1] = tostring(m) end
  end
  table.sort(models)
  local new_providers = {}
  local found = false
  for _, prov in ipairs(next_state.popup.providers or {}) do
    if prov.name == provider then
      new_providers[#new_providers + 1] = shallow_merge(prov, { models = models })
      found = true
    else
      new_providers[#new_providers + 1] = prov
    end
  end
  if not found then
    new_providers[#new_providers + 1] = {
      name = provider, state = state.auth and state.auth[provider] or "unknown",
      models = models,
    }
    table.sort(new_providers, function(a, b) return a.name < b.name end)
  end
  local prev_awaiting = next_state.popup.awaiting or {}
  local new_awaiting = {}
  for k, v in pairs(prev_awaiting) do new_awaiting[k] = v end
  new_awaiting[provider] = nil
  return shallow_merge(next_state, {
    popup = shallow_merge(next_state.popup, {
      providers = new_providers,
      awaiting  = new_awaiting,
    }),
  }), {}
end

local function handle_auth_status(msg, state)
  local provider = msg.provider or ""
  local status = msg.status or msg.state or "unknown"
  if provider == "" then return state, {} end
  local auth = {}
  for k, v in pairs(state.auth or {}) do auth[k] = v end
  auth[provider] = status
  local supports = {}
  for k, v in pairs(state.supports_login or {}) do supports[k] = v end
  if msg.supports_login ~= nil then
    supports[provider] = msg.supports_login and true or false
  end
  local supports_usage = {}
  for k, v in pairs(state.supports_usage or {}) do supports_usage[k] = v end
  if msg.supports_usage ~= nil then
    supports_usage[provider] = msg.supports_usage and true or false
  end
  local usage = {}
  for k, v in pairs(state.usage or {}) do usage[k] = v end
  if status ~= "connected" then usage[provider] = nil end
  local new_popup = state.popup
  if state.popup and state.popup.variant == "model_picker"
     and state.popup.providers then
    local found_section = false
    local new_providers = {}
    for _, prov in ipairs(state.popup.providers) do
      if prov.name == provider then
        found_section = true
        new_providers[#new_providers + 1] = shallow_merge(prov, { state = status })
      else
        new_providers[#new_providers + 1] = prov
      end
    end
    if not found_section then
      new_providers[#new_providers + 1] = {
        name = provider, state = status, models = {},
      }
      table.sort(new_providers, function(a, b) return a.name < b.name end)
    end
    new_popup = shallow_merge(state.popup, { providers = new_providers })
  end
  return shallow_merge(state, {
    auth = auth,
    supports_login = supports,
    supports_usage = supports_usage,
    usage = usage,
    popup = new_popup,
  }), {}
end

local function handle_tool_popup_request(msg, state)
  if state.replay_mode then return state, {} end
  local args = msg.args
  local body
  if msg.input_pretty ~= nil then
    body = tostring(msg.input_pretty)
  elseif type(args) == "table" then
    body = format_args(args)
  elseif args ~= nil then
    body = tostring(args)
  else
    body = ""
  end
  local new_popup = {
    variant = "tool_permission",
    tool    = msg.tool or msg.name or "?",
    id      = msg.id,
    body    = body,
    source  = msg.source,
  }
  return enqueue_permission_popup(state, new_popup), {}
end

local function handle_mag_approval_request(msg, state)
  if state.replay_mode then return state, {} end
  if type(msg.run_id) ~= "string" or msg.run_id == ""
      or type(msg.from) ~= "string" or msg.from == ""
      or type(msg.correlation) ~= "string" or msg.correlation == "" then
    return state, {}
  end
  local subject = msg.subject
  local body = msg.prompt
  if type(body) ~= "string" or body == "" then
    if type(subject) == "table" then body = format_args(subject)
    elseif subject ~= nil then body = tostring(subject)
    else body = "Approval requested"
    end
  end
  return enqueue_permission_popup(state, {
    variant = "tool_permission",
    permission_kind = "mag_approval",
    tool = msg.prompt or "Human approval",
    body = body,
    id = msg.correlation,
    correlation = msg.correlation,
    run_id = msg.run_id,
    gate_id = msg.from,
    subject = msg.subject,
  }), {}
end

local function handle_mag_approval_cancel(msg, state)
  if type(msg.run_id) ~= "string" or type(msg.correlation) ~= "string" then
    return state, {}
  end
  return retract_mag_approvals(state, msg.run_id, msg.correlation), {}
end

local function handle_gate_mode_changed(msg, state)
  local mode = msg.mode
  if mode == "normal" then mode = "safe" end
  if mode ~= "safe" and mode ~= "auto" and mode ~= "yolo" then return state, {} end
  return shallow_merge(state, { gate_mode = mode }), {}
end

-- ── MAG kernel observation ────────────────────────────────────────────
--
-- The mag plugin relays the kernel's `mag.*` lifecycle stream onto the bus
-- as broadcasts; the chat surface consumes them to drive the sidebar run
-- panel. Kernel runs are concurrent
-- (the lead's own turn-program overlaps its dispatched sub-runs), and
-- every event carries its `run_id` — panel state keys straight off it,
-- so overlapping runs render independently.

local function handle_mag_contracts(msg, state)
  return preview_state.advertise(state, msg.foreign_contracts), {}
end

local function handle_mag_node_preview(msg, state)
  if state.replay_mode then return state, {} end
  local next = preview_state.observe(state, msg, tui.now_ms())
  if next ~= state and state.popup and state.popup.variant == "node_inspector"
      and state.popup.run_id == msg.run_id
      and (state.popup.actor_id == nil or state.popup.actor_id == msg.id) then
    local at_end = false
    local ok, position = pcall(tui.scroll_position, "popup_node_inspector")
    if ok and type(position) == "table" then
      at_end = (position.offset or 0) >= math.max(0,
        (position.content_height or 0) - (position.viewport_height or 0))
    end
    if not at_end then
      next = shallow_merge(next, { popup = shallow_merge(state.popup, { unseen = true }) })
    end
  end
  return next, {}
end

local function handle_mag_modification_applied(msg, state)
  if state.replay_mode then return state, {} end
  return preview_state.apply_modification(state, msg, tui.now_ms()), {}
end

local function handle_mag_run_started(msg, state)
  local run_id = msg.run_id
  if type(run_id) ~= "string" or run_id == "" then return state, {} end
  run_bindings.bind(msg, msg._event_source)
  -- Scope bindings are safe replay state: lead instruction notices need the
  -- same run/scope validation while rebuilding the transcript. Historical run
  -- panels and agent streams remain intentionally unreconstructed.
  state = preview_state.set_scope(state, msg.scope, run_id)
  if state.replay_mode then return state, {} end
  return run_panel.mag_run_started(state, run_id, msg.run_name, msg.principal, tui.now_ms()), {}
end

local function handle_queue_steered(_msg, state)
  local next_state, changed = queued_input.accept_steered(state)
  if not changed then return state, {} end
  height_cache.invalidate_all()
  return next_state, {}
end

local function handle_mag_actor_spawned(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  local id = msg.id
  if type(run_id) ~= "string" or type(id) ~= "string" or id == "" then return state, {} end
  local next = run_panel.actor_spawned(state, run_id, id, msg.factory, tui.now_ms())
  return preview_state.spawn(next, run_id, id, msg.factory, tui.now_ms()), {}
end

local function handle_mag_actor_ready(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  local id = msg.id
  if type(run_id) ~= "string" or type(id) ~= "string" or id == "" then return state, {} end
  local now = tui.now_ms()
  return preview_state.lifecycle(run_panel.actor_ready(state, run_id, id, now), run_id, id, "idle", now), {}
end

-- The kernel's per-actor activity window (mag.actor_busy / mag.actor_idle):
-- members tick only while actually working, so the panel shows the loop
-- cycling instead of every row mirroring the run's wall clock.
local function handle_mag_actor_busy(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  local id = msg.id
  if type(run_id) ~= "string" or type(id) ~= "string" or id == "" then return state, {} end
  local now = tui.now_ms()
  return preview_state.lifecycle(run_panel.actor_busy(state, run_id, id, now), run_id, id, "working", now), {}
end

local function handle_mag_actor_idle(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  local id = msg.id
  if type(run_id) ~= "string" or type(id) ~= "string" or id == "" then return state, {} end
  local now = tui.now_ms()
  return preview_state.lifecycle(run_panel.actor_idle(state, run_id, id, now), run_id, id, "idle", now), {}
end

local function handle_mag_actor_killed(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  local id = msg.id
  if type(run_id) ~= "string" or type(id) ~= "string" or id == "" then return state, {} end
  local now = tui.now_ms()
  return preview_state.lifecycle(run_panel.actor_killed(state, run_id, id, now, msg.reason), run_id, id, msg.reason == "run_failed" and "failed" or "killed", now), {}
end

local function handle_mag_modification_rejected(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  if type(run_id) ~= "string" then return state, {} end
  return run_panel.modification_rejected(state, run_id), {}
end

local function handle_mag_modification_noop(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  if type(run_id) ~= "string" then return state, {} end
  return run_panel.modification_noop(state, run_id), {}
end

local function handle_mag_run_complete(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  if type(run_id) ~= "string" then return state, {} end
  local now = tui.now_ms()
  local next = preview_state.finish_run(run_panel.mag_run_complete(state, run_id, "success", now), run_id, "done", now)
  return retract_mag_approvals(next, run_id), {}
end

-- A run that ended on an unhandled failure. Same linger→prune bookkeeping as
-- run_complete (without it a failed run lingers in the sidebar forever), but
-- marks the run failed so its members read red.
local function handle_mag_run_failed(msg, state)
  if state.replay_mode then return state, {} end
  local run_id = msg.run_id
  if type(run_id) ~= "string" then return state, {} end
  local now = tui.now_ms()
  local next = preview_state.finish_run(run_panel.mag_run_failed(state, run_id, "failed", now), run_id, "failed", now)
  return retract_mag_approvals(next, run_id), {}
end

local function handle_mag_run_result(msg, state)
  if type(msg.run_id) ~= "string" then return state, {} end
  local next = retract_mag_approvals(state, msg.run_id)
  local run = (state.runs or {})[msg.run_id]
  if run ~= nil and run.principal == "lead" then
    next = transcript.close_lead_unit(next)
  end
  return next, {}
end

-- The tool gate broadcasts `tool-gate.tool.invoke { id, from, name, args }`

local function handle_mouse_selection(msg, state)
  local text = msg.text or ""
  if #text > 0 then
    local now = tui.now_ms()
    tui.copy_to_clipboard(text)
    local toasts = {}
    for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
    toasts[#toasts + 1] = {
      id            = "clipboard-" .. tostring(now),
      text          = string.format("copied %d chars", #text),
      level         = "info",
      started_at_ms = now,
      ttl_ms        = 4000,
    }
    return shallow_merge(state, { toasts = toasts }), {}
  end
  return state, {}
end

-- ── dispatch table ────────────────────────────────────────────────────

local handlers = {
  ["input.changed"]               = handle_input_changed,
  ["input.submit"]                = handle_input_submit,
  ["key.ctrl_c"]                  = handle_ctrl_c,
  ["chat.exit_timeout"]           = handle_exit_timeout,
  ["key.ctrl_d"]                  = exit_effects,
  ["key.ctrl_b"]                  = handle_toggle_sidebar,
  ["key.tab"]                     = handle_focus_cycle,
  ["key.shift_tab"]               = handle_focus_cycle,
  ["key.ctrl_o"]                  = handle_toggle_expand,
  ["key.ctrl_r"]                  = handle_toggle_tool_raw,
  ["key.?"]                       = handle_help_key,
  ["key.shift_?"]                 = handle_help_key,
  ["key.escape"]                  = handle_escape,
  ["chat.escape_timeout"]         = handle_escape_timeout,
  ["sessions.session_end"]        = handle_session_end,
  ["sessions.session_start"]      = handle_session_start,
  ["sessions.resume_loading"]     = handle_resume_loading,
  ["sessions.replay.start"]       = handle_replay_start,
  ["sessions.replay.progress"]    = handle_replay_progress,
  ["sessions.replay.end"]         = handle_replay_end,
  ["sessions.resume_done"]        = handle_resume_done,
  ["chat.reset"]                  = handle_chat_reset,
  ["chat.message.append"]         = handle_message_append,
  ["chat.error.append"]           = handle_error_append,
  ["chat.queue.steered"]          = handle_queue_steered,
  ["chat.instruction.notice"]     = handle_instruction_notice,
  ["chat.lead.bound"]             = handle_lead_chat_bound,
  ["chat.stream.delta"]           = handle_stream_delta,
  ["chat.stream.end"]             = handle_stream_end,
  ["chat.stream.reasoning_delta"] = handle_reasoning_delta,
  ["chat.stream.reasoning_end"]   = handle_reasoning_end,
  ["chat.session.stats"]          = handle_session_stats,
  ["chat.usage.updated"]          = handle_usage_updated,
  ["chat.usage.error"]            = handle_usage_error,
  ["tool.register"]               = handle_tool_register,
  ["chat.tool.start"]             = handle_tool_start,
  ["chat.tool.end"]               = handle_tool_end,
  ["chat.graph_result.append"]    = handle_graph_result_append,
  ["chat.plan.append"]            = handle_plan_append,
  ["chat.compaction.commit"]      = handle_compaction_commit,
  ["chat.compaction.failed"]      = handle_compaction_failed,
  ["lead-workflow.plan.approved"] = handle_plan_approved,
  ["chat.popup"]                  = handle_popup,
  ["chat.toast"]                  = handle_toast,
  ["chat.model.set_ack"]          = handle_model_set_ack,
  ["chat.reasoning.set_ack"]      = handle_reasoning_set_ack,
  ["chat.models.listed"]          = handle_models_listed,
  ["chat.auth.status"]            = handle_auth_status,
  ["chat.tool.popup_request"]     = handle_tool_popup_request,
  ["tool-gate.mode_changed"]      = handle_gate_mode_changed,
  ["mag.hello"]                   = handle_mag_contracts,
  ["mag.loaded"]                  = handle_mag_contracts,
  ["mag.run_started"]             = handle_mag_run_started,
  ["mag.approval_request"]        = handle_mag_approval_request,
  ["mag.approval_cancel"]         = handle_mag_approval_cancel,
  ["mag.node_preview"]            = handle_mag_node_preview,
  ["mag.modification_applied"]    = handle_mag_modification_applied,
  ["mag.actor_spawned"]           = handle_mag_actor_spawned,
  ["mag.actor_ready"]             = handle_mag_actor_ready,
  ["mag.actor_busy"]              = handle_mag_actor_busy,
  ["mag.actor_idle"]              = handle_mag_actor_idle,
  ["mag.actor_killed"]            = handle_mag_actor_killed,
  ["mag.modification_rejected"]   = handle_mag_modification_rejected,
  ["mag.modification_noop"]       = handle_mag_modification_noop,
  ["mag.run_complete"]            = handle_mag_run_complete,
  ["mag.run_failed"]              = handle_mag_run_failed,
  ["mag.run_result"]              = handle_mag_run_result,
  ["mouse.selection"]             = handle_mouse_selection,
}

-- ── key / popup / scroll routing (fall-through for non-dispatched) ────

local function route_keys_and_popups(msg, state)
  local kind = msg.kind or ""

  if state.popup and state.popup.variant == "terminate_workflow" then
    if kind == "key.escape" or kind == "key.n" or kind == "key.N"
        or kind == "key.q" or kind == "key.Q" then
      return shallow_merge(state, { popup = NIL_SENTINEL }), {}
    end
    if kind == "key.enter" or kind == "key.y" or kind == "key.Y" then
      local p = state.popup
      local next_state = shallow_merge(state, { popup = NIL_SENTINEL })
      return apply_control_decisions(workflow_controls.confirm_termination(next_state, p))
    end
    return state, {}
  end

  -- Info / warning / error popups are dismiss-only and accept Esc,
  -- Enter, or Q.
  if state.popup
     and (state.popup.variant == "info"
       or state.popup.variant == "warning"
       or state.popup.variant == "error")
     and (kind == "key.escape" or kind == "key.enter"
       or kind == "key.q" or kind == "key.Q") then
    return shallow_merge(state, { popup = NIL_SENTINEL }), {}
  end

  -- Node inspector: read-only, so dismiss, scroll, and the shared detail
  -- toggle are the only verbs. Keeping Ctrl+O aligned with lead chat makes
  -- full reasoning/tool payloads deliberately accessible without making
  -- debug-shaped output the default.
  if state.popup and state.popup.variant == "node_inspector"
      and kind == "key.ctrl_o" then
    return handle_toggle_expand(msg, state)
  end
  if state.popup and state.popup.variant == "node_inspector"
     and (kind == "key.q" or kind == "key.Q") then
    return shallow_merge(state, { popup = NIL_SENTINEL }), {}
  end

  -- Tool permission popup keys.
  if state.popup and state.popup.variant == "tool_permission" then
    if kind == "key.a" or kind == "key.A" or kind == "key.enter" then
      return resolve_permission(state, true)
    end
    if kind == "key.d" or kind == "key.D" then
      return resolve_permission(state, false)
    end
  end

  -- Model picker popup.
  if state.popup and state.popup.variant == "model_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local q_lc = (p.query or ""):lower()
    local flat_rows = {}
    for _, prov in ipairs(p.providers or {}) do
      for _, m in ipairs(prov.models or {}) do
        local s = tostring(m):lower()
        if q_lc == "" or s:find(q_lc, 1, true) ~= nil then
          flat_rows[#flat_rows + 1] = { provider = prov.name, model = m }
        end
      end
    end
    local result = W.picker.handle({
      state   = { cursor = p.cursor or 1, query = p.query or "" },
      entries = function() return flat_rows end,
      filter  = function(entries, _q) return entries end,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil then
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = {
              kind     = "chat.model.set",
              provider = result.selected.provider,
              model    = result.selected.model,
            } },
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
  end

  -- Login/logout picker.
  if state.popup and state.popup.variant == "login_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local rows = p.providers or {}
    local result = W.picker.handle({
      state       = { cursor = p.cursor or 1 },
      entries     = function() return rows end,
      show_search = false,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil and result.selected.name then
        local mode = p.mode or "login"
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = {
              kind     = "chat." .. mode .. "_requested",
              provider = result.selected.name,
            } },
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
    return state, {}
  end

  -- Session picker.
  if state.popup and state.popup.variant == "session_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local rows = p.sessions or {}
    local result = W.picker.handle({
      state       = { cursor = p.cursor or 1 },
      entries     = function() return rows end,
      show_search = false,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil and result.selected.id then
        reset_transcript_scroll()
        return shallow_merge(state, {
          popup = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending_assistant_projection = NIL_SENTINEL,
          pending = false, runs = {}, sidebar_folds = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
          queued_entry_idx = NIL_SENTINEL,
          resume_loading = { session_id = result.selected.id, replayed = 0 },
          replay_mode = true,
        }), {
          sessions.emit_resume_request(result.selected.id),
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
    return state, {}
  end

  -- Slash and @-path autocomplete keys (when completion popup open).
  if state.completion ~= nil then
    local result = W.prompt.handle(prompt_widget_opts(state), msg)
    if result ~= nil then
      return fold_prompt_patch(state, result.state or {}), {}
    end
  end

  -- Sidebar pane focus (popups above win the keyboard first). Keys split
  -- by concern: Enter = STRUCTURE (fold/unfold a group row only; a no-op
  -- on leaf and run-header rows), Space = OBSERVATION (toggle the row's
  -- observability view uniformly — a leaf's own timeline, a group's merged
  -- member timeline, a run-header's whole-run merged timeline). Up/Down
  -- move the cursor over the row model (folded members are not rows, so
  -- the cursor skips them by construction).
  if state.focus == "sidebar" and state.popup == nil then
    local now = tui.now_ms()
    if kind == "key.x" or kind == "key.X" then
      if kind == "key.X" then
        return shallow_merge(state, {
          popup = { variant = "terminate_workflow", scope = "all" },
        }), {}
      end
      local rows = run_panel.row_model(state, now)
      local row = rows[run_panel.clamp_cursor(state.sidebar_cursor, #rows)]
      if row == nil then return state, {} end
      local run = (state.runs or {})[row.run_id]
      if not run or run.completed_at_ms ~= nil then return state, {} end
      return shallow_merge(state, {
        popup = {
          variant = "terminate_workflow",
          scope = "one",
          run_id = row.run_id,
          label = run.run_name or row.run_id,
        },
      }), {}
    end
    if kind == "key.up" or kind == "key.down" then
      local rows = run_panel.row_model(state, now)
      if #rows == 0 then return state, {} end
      local cur = run_panel.clamp_cursor(state.sidebar_cursor, #rows)
      -- Wheel scrolling is passive: it moves only the viewport. On the next
      -- navigation key, resume from the nearest visible row rather than
      -- snapping the viewport back to an off-screen cursor.
      local pos = tui.scroll_position("sidebar")
      local viewport_start = pos.offset
      local viewport_end = pos.offset + pos.viewport_size
      local selected = rows[cur]
      if selected.visual_end <= viewport_start then
        for i, row in ipairs(rows) do
          if row.visual_end > viewport_start then cur = i break end
        end
      elseif selected.visual_start >= viewport_end then
        for i = #rows, 1, -1 do
          if rows[i].visual_start < viewport_end then cur = i break end
        end
      end
      cur = cur + ((kind == "key.down") and 1 or -1)
      if cur < 1 then cur = 1 elseif cur > #rows then cur = #rows end
      local row = rows[cur]
      tui.scroll_reveal("sidebar", row.visual_start, row.visual_end)
      return shallow_merge(state, { sidebar_cursor = cur }), {}
    end
    if kind == "key.home" or kind == "key.end" then
      local rows = run_panel.row_model(state, now)
      if #rows == 0 then return state, {} end
      local cur = (kind == "key.home") and 1 or #rows
      local row = rows[cur]
      if kind == "key.home" then
        tui.scroll_to("sidebar", 0)
      else
        tui.scroll_into_view("sidebar")
      end
      tui.scroll_reveal("sidebar", row.visual_start, row.visual_end)
      return shallow_merge(state, { sidebar_cursor = cur }), {}
    end
    if kind == "key.pageup" or kind == "key.pagedown" then
      local rows = run_panel.row_model(state, now)
      if #rows == 0 then return state, {} end
      local cur = run_panel.clamp_cursor(state.sidebar_cursor, #rows)
      cur = cur + ((kind == "key.pagedown") and 10 or -10)
      if cur < 1 then cur = 1 elseif cur > #rows then cur = #rows end
      local row = rows[cur]
      tui.scroll_reveal("sidebar", row.visual_start, row.visual_end)
      return shallow_merge(state, { sidebar_cursor = cur }), {}
    end
    if kind == "key.enter" then
      local rows = run_panel.row_model(state, now)
      local row = rows[run_panel.clamp_cursor(state.sidebar_cursor, #rows)]
      if row ~= nil and row.kind == "group" then
        return run_panel.toggle_fold(state, row.run_id, row.group.name), {}
      end
      -- Leaf and run-header Enter are no-ops: view-opening moved to Space.
      return state, {}
    end
    if kind == "key.space" then
      local rows = run_panel.row_model(state, now)
      local row = rows[run_panel.clamp_cursor(state.sidebar_cursor, #rows)]
      if row == nil then
        local run_id = run_panel.recent_completed(state.runs, now)
        if run_id == nil then return state, {} end
        return shallow_merge(state, {
          popup = { variant = "node_inspector", run_id = run_id,
            whole_run = true, completed_archive = true },
        }), {}
      end
      if row.kind == "actor" then
        return shallow_merge(state, {
          popup = { variant = "node_inspector", run_id = row.run_id, actor_id = row.actor_id },
        }), {}
      elseif row.kind == "group" then
        return shallow_merge(state, {
          popup = { variant = "node_inspector", run_id = row.run_id, group = row.group.name },
        }), {}
      elseif row.kind == "run_header" then
        -- Run-header Space observes the WHOLE run merged (every actor
        -- under it) — the natural "observe this run" verb, more useful
        -- than a no-op.
        return shallow_merge(state, {
          popup = { variant = "node_inspector", run_id = row.run_id, whole_run = true },
        }), {}
      end
      return state, {}
    end
  end

  -- Scroll keys.
  local function active_scroll_key()
    if state.popup then return popups.scroll_key(state.popup.variant) end
    return nil
  end

  local function route_scroll(delta_or_fn)
    local target = active_scroll_key() or "transcript"
    delta_or_fn(target)
  end

  if kind == "key.pageup" then
    route_scroll(function(k) tui.scroll_by(k, -10) end)
    return state, {}
  end
  if kind == "key.pagedown" then
    route_scroll(function(k) tui.scroll_by(k, 10) end)
    return state, {}
  end
  if kind == "key.up" or kind == "key.down" then
    if active_scroll_key() == nil then
      local result = W.prompt.handle(prompt_widget_opts(state), msg)
      if result ~= nil then
        return fold_prompt_patch(state, result.state or {}), {}
      end
    end
    local delta = (kind == "key.up") and -1 or 1
    route_scroll(function(k) tui.scroll_by(k, delta) end)
    return state, {}
  end
  if kind == "key.home" then
    route_scroll(function(k) tui.scroll_to(k, 0) end)
    return state, {}
  end
  if kind == "key.end" then
    route_scroll(function(k) tui.scroll_into_view(k) end)
    if state.popup and state.popup.variant == "node_inspector" and state.popup.unseen then
      return shallow_merge(state, { popup = shallow_merge(state.popup, { unseen = false }) }), {}
    end
    return state, {}
  end

  return state, {}
end

-- ── main entry point ──────────────────────────────────────────────────

function M.update(msg, state)
  state = prune_expired(state)
  local kind = msg.kind or ""
  local user_action = kind:sub(1, 4) == "key."
    or kind:sub(1, 6) == "input."
    or kind:sub(1, 6) == "mouse."
  if user_action and kind ~= "key.ctrl_c" then
    state = exit_controls.reset(state)
  end
  log.log("update", "dispatch kind=%s", kind)
  local handler = handlers[kind]
  local next_state, effects
  if handler then
    next_state, effects = handler(msg, state)
  else
    next_state, effects = route_keys_and_popups(msg, state)
  end
  -- Model and geometry can change without a navigation key (actor events,
  -- folds, linger pruning). Keep the focused selection valid and visible.
  -- Mouse wheel events bypass Lua dispatch, so this does not fight passive
  -- viewport scrolling; the next keyboard event deliberately resumes from
  -- the nearest visible row in route_keys_and_popups.
  if next_state.focus == "sidebar" then
    local start, finish, cursor = run_panel.selected_visual_range(next_state, tui.now_ms())
    if cursor ~= (next_state.sidebar_cursor or 1) then
      next_state = shallow_merge(next_state, { sidebar_cursor = cursor })
    end
    local model_changed = kind:match("^mag%.") ~= nil or kind == "key.enter"
      or state.focus ~= next_state.focus
    if start ~= nil and model_changed then
      tui.scroll_reveal("sidebar", start, finish)
    end
  end
  return transcript.flush_graph_results_if_stable(next_state), effects
end

return M
