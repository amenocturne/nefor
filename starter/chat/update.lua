-- Reducer for the chat surface. Receives a message + current state,
-- returns (next_state, effects). Effects are NCP envelopes the engine
-- routes onto the bus. Pure update except for `tui.now_ms` reads and
-- `tui.scroll_*` / `tui.copy_to_clipboard` side-effect bindings.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget

local common       = require("chat.common")
local slash        = require("chat.slash")
local sessions     = require("chat.sessions")
local at_path      = require("chat.at_path")
local history      = require("chat.history")
local dag          = require("chat.dag")
local transcript   = require("chat.transcript")
local popups       = require("chat.popups")
local Entry        = require("chat.entry")
local log          = require("chat.log")
local height_cache = require("chat.height_cache")

local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL
local format_args   = common.format_args

local M = {}

local DOUBLE_ESC_MS = 600

local function pop_next_popup(state_tbl)
  local queue = state_tbl.popup_queue
  if type(queue) == "table" and #queue > 0 then
    local next_popup = table.remove(queue, 1)
    if #queue == 0 then queue = NIL_SENTINEL end
    return { popup = next_popup, popup_queue = queue }
  end
  return { popup = NIL_SENTINEL }
end

-- Per-model context window sizes reported by the provider's model list.
-- Populated by chat.models.listed events; keyed by model id.
local model_context_windows = {}

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

-- Pure-update prune for stale dag runs + expired toasts.
local function prune_expired(state)
  local now = tui.now_ms()
  local pruned = dag.prune(state.dag_runs or {}, now)
  if pruned ~= state.dag_runs then
    state = shallow_merge(state, { dag_runs = pruned })
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
    local cleared = shallow_merge(state, {
      entries = {}, in_flight = NIL_SENTINEL, input_value = "",
      pending = false, completion = NIL_SENTINEL,
      dag_runs = {}, firing_to_node = {},
      turn_started_at = NIL_SENTINEL,
      last_turn_duration_ms = NIL_SENTINEL,
      last_esc_ms = NIL_SENTINEL,
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
  if cmd == "yolo" then
    local s = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
    return s, {
      { kind = "send_to", target = "engine",
        body = { kind = "tool-gate.set_mode", mode = "yolo" } },
    }
  end
  if cmd == "safe" then
    local s = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
    return s, {
      { kind = "send_to", target = "engine",
        body = { kind = "tool-gate.set_mode", mode = "normal" } },
    }
  end
  if cmd == "debug" then
    local chatlog = require("chat.log")
    if chatlog.is_enabled() then chatlog.disable() else chatlog.enable() end
    local toasts = {}
    for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
    toasts[#toasts + 1] = {
      id = "debug-" .. tostring(tui.now_ms()),
      text = chatlog.is_enabled() and "debug logging ON" or "debug logging OFF",
      level = "info",
      started_at_ms = tui.now_ms(),
      ttl_ms = 2000,
    }
    return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL, toasts = toasts }), {}
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
  if cmd == "resume" then
    if args and #args > 0 then
      local id = args:match("^([%w%-]+)") or args
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        entries = {}, in_flight = NIL_SENTINEL,
        pending = false, dag_runs = {}, firing_to_node = {},
        turn_started_at = NIL_SENTINEL,
        last_turn_duration_ms = NIL_SENTINEL,
        queued_entry_idx = NIL_SENTINEL,
      }), {
        sessions.emit_resume_request(id),
      }
    end
    local rows = sessions.list_recent(10)
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = {
        variant  = "session_picker",
        sessions = rows,
        cursor   = 1,
      },
    }), {}
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
    local next_state
    if state.queued_entry_idx then
      local old = state.entries[state.queued_entry_idx]
      local combined = Entry.set_text(old, old.text .. "\n" .. wire_text)
      local new_entries = {}
      for ei = 1, #state.entries do
        new_entries[ei] = (ei == state.queued_entry_idx) and combined or state.entries[ei]
      end
      next_state = shallow_merge(state, {
        entries = new_entries,
        input_value = "", completion = NIL_SENTINEL,
        prompt_history = hist, history_cursor = NIL_SENTINEL,
      })
    else
      local with_user = transcript.push_entry(state, Entry.user(wire_text))
      next_state = shallow_merge(with_user, {
        input_value = "", completion = NIL_SENTINEL,
        prompt_history = hist, history_cursor = NIL_SENTINEL,
        queued_entry_idx = #with_user.entries,
      })
    end
    tui.scroll_into_view("transcript")
    return next_state, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = wire_text } },
    }
  end

  local with_user = transcript.push_entry(state, Entry.user(wire_text))
  local cleared = shallow_merge(with_user, {
    input_value = "", pending = true,
    turn_started_at = tui.now_ms(), completion = NIL_SENTINEL,
    prompt_history = hist,
    history_cursor = NIL_SENTINEL,
    pending_user_echo = wire_text,
  })
  tui.scroll_into_view("transcript")
  return cleared, {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.input.submit", text = wire_text } },
  }
end

local function handle_exit(_msg, state)
  return state, {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.interrupt_all" } },
    { kind = "exit" },
  }
end

local function handle_toggle_sidebar(_msg, state)
  return shallow_merge(state, { show_sidebar = not state.show_sidebar }), {}
end

local function handle_toggle_expand(_msg, state)
  if type(tui.virtual_scroll_invalidate) == "function" then
    tui.virtual_scroll_invalidate("chat")
  end
  height_cache.invalidate_all()
  return shallow_merge(state, { expanded_details = not state.expanded_details }), {}
end

local function handle_help_key(_msg, state)
  if state.input_value == "" then
    return shallow_merge(state, { popup = { variant = "help" } }), {}
  end
  return state, {}
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
      local id = state.popup.id
      return shallow_merge(state, pop_next_popup(state)), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "deny" } },
      }
    end
    return shallow_merge(state, { popup = NIL_SENTINEL, toasts = {} }), {}
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
  -- 4) double-ESC escalation
  local now = tui.now_ms()
  if state.last_esc_ms and (now - state.last_esc_ms) <= DOUBLE_ESC_MS then
    local interrupted = dag.interrupt_all(state, now)
    return shallow_merge(interrupted, { last_esc_ms = NIL_SENTINEL }), {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.interrupt_all" } },
    }
  end
  -- 4) single ESC interrupts the current turn
  if state.pending or state.in_flight ~= nil then
    local interrupted = dag.interrupt_all(state, now)
    return shallow_merge(interrupted, { last_esc_ms = now }), {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.interrupt" } },
    }
  end
  -- Stamp anyway so a follow-up ESC within the window can escalate.
  return shallow_merge(state, { last_esc_ms = now }), {}
end

-- ── session lifecycle ─────────────────────────────────────────────────

local function handle_session_end(_msg, state)
  return shallow_merge(state, {
    in_flight        = NIL_SENTINEL,
    pending          = false,
    turn_started_at  = NIL_SENTINEL,
    last_turn_duration_ms = NIL_SENTINEL,
    popup            = NIL_SENTINEL,
    toasts           = {},
    completion       = NIL_SENTINEL,
    dag_runs         = {},
  }), {}
end

local function handle_session_start(_msg, state)
  return shallow_merge(state, { dag_runs = {}, firing_to_node = {} }), {}
end

local function handle_replay_start(_msg, state)
  return shallow_merge(state, { replay_mode = true }), {}
end

local function handle_replay_end(_msg, state)
  return shallow_merge(state, { replay_mode = NIL_SENTINEL }), {}
end

local function handle_chat_reset(_msg, state)
  return state, {}
end

-- ── inbound chat-contract events ──────────────────────────────────────

local function handle_message_append(msg, state)
  local text = msg.text or ""
  if #text == 0 then return state, {} end
  local role = msg.role or "system"
  -- Round-trip echo dedup.
  if role == "user"
     and state.pending_user_echo ~= nil
     and state.pending_user_echo == text then
    local entries = state.entries or {}
    local tail = entries[#entries]
    local local_push_landed = tail
      and tail.role == "user"
      and tail.text == text
    if local_push_landed then
      return shallow_merge(state, { pending_user_echo = NIL_SENTINEL }), {}
    end
    return transcript.push_entry(
      shallow_merge(state, { pending_user_echo = NIL_SENTINEL }),
      Entry.user(text)
    ), {}
  end
  local turn_state = role == "system"
    and { pending = false, turn_started_at = NIL_SENTINEL }
    or  {}

  -- AGENTS.md auto-load routing.
  if role == "system" then
    local path, dir = text:match(
      "^%[Loaded (.-) because tool call touched a file in (.-)%. This is project guidance for that directory, not a user request%.%]")
    if path and dir then
      local mc = msg.chat_id
      if type(mc) == "string" and #mc > 0 then
        local binding = state.chat_id_to_node and state.chat_id_to_node[mc]
        if binding then
          local now = tui.now_ms()
          local synth = "AGENTS.md(" .. path .. ")"
          return shallow_merge(
            dag.node_tool_invoked(state, binding.run_id, binding.node_id, synth, nil, now),
            turn_state
          ), {}
        end
        return shallow_merge(state, turn_state), {}
      end
      local body = text:match("\n\n(.*)$") or ""
      return transcript.push_entry(shallow_merge(state, turn_state),
        Entry.agents_md(path, dir, body)
      ), {}
    end
  end

  if role == "system" then
    return transcript.push_entry(shallow_merge(state, turn_state),
      Entry.system(text)
    ), {}
  end
  if role == "assistant" then
    return transcript.push_entry(shallow_merge(state, turn_state),
      Entry.assistant(text)
    ), {}
  end
  return transcript.push_entry(shallow_merge(state, turn_state), {
    role = role, text = text, kind = "text",
  }), {}
end

local function handle_stream_delta(msg, state)
  local t = msg.text or msg.delta or ""
  if #t == 0 then return state, {} end
  return transcript.append_assistant_delta(state, t), {}
end

local function handle_stream_end(msg, state)
  local next_state = transcript.finalize_assistant(state, msg.text, msg.model, msg.duration_ms)
  if state.queued_entry_idx then
    local qe = next_state.entries[state.queued_entry_idx]
    next_state = shallow_merge(next_state, {
      queued_entry_idx = NIL_SENTINEL,
      pending_user_echo = qe and qe.text or NIL_SENTINEL,
    })
  end
  return next_state, {}
end

local function handle_reasoning_delta(msg, state)
  local t = msg.text or msg.delta or ""
  if #t == 0 then return state, {} end
  return transcript.append_reasoning_delta(state, t), {}
end

local function handle_reasoning_end(msg, state)
  return transcript.finalize_reasoning(state, msg.duration_ms), {}
end

local function handle_session_stats(msg, state)
  local stats = shallow_merge(state.stats or {}, {})
  for k, v in pairs(msg) do
    if k ~= "kind" then stats[k] = v end
  end
  local s = shallow_merge(state, { stats = stats })
  if msg.model then
    local mt = msg.max_context_tokens
      or model_context_windows[msg.model]
      or state.max_tokens
    s = shallow_merge(s, { model = msg.model, max_tokens = mt })
  end
  return s, {}
end

local function handle_tool_start(msg, state)
  local input_str
  if type(msg.input) == "string" then input_str = msg.input
  elseif type(msg.input) == "table" then input_str = "(object)"
  else input_str = "" end
  return transcript.push_entry(state,
    Entry.tool_call(msg.id or "", msg.name or "?", input_str,
      type(msg.input) == "table" and msg.input or nil)
  ), {}
end

local function handle_tool_end(msg, state)
  return transcript.attach_tool_end(state, msg.id or "", msg.output or "", msg.error == true), {}
end

local function handle_graph_result_append(msg, state)
  return transcript.push_entry(state,
    Entry.graph_result(
      msg.run_id or "",
      msg.status or "success",
      msg.nodes or {},
      msg.output,
      msg.error,
      msg.duration_ms)
  ), {}
end

local function handle_plan_append(msg, state)
  local text = msg.text or ""
  if #text == 0 then return state, {} end
  local submitted_at = msg.submitted_at
  if submitted_at ~= nil then
    for _, v in ipairs(state.entries) do
      if v.kind == "plan" and v.submitted_at == submitted_at then
        return state, {}
      end
    end
  end
  return transcript.push_entry(state,
    Entry.plan(text, submitted_at)
  ), {}
end

local function handle_plan_approved(msg, state)
  local approved = (msg.approved == true)
  local entries = {}
  local target_idx
  for i, v in ipairs(state.entries) do
    if v.kind == "plan" and v.status == "pending" then
      target_idx = i
    end
    entries[i] = v
  end
  if target_idx == nil then return state, {} end
  entries[target_idx] = shallow_merge(entries[target_idx], {
    status = approved and "approved" or "rejected",
  })
  return shallow_merge(state, { entries = entries }), {}
end

local function handle_popup(msg, state)
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
  return shallow_merge(state, {
    model = msg.model or state.model,
    max_tokens = model_context_windows[msg.model] or state.max_tokens,
  }), {}
end

local function handle_models_listed(msg, state)
  -- Absorb per-model context windows if the provider reported them.
  if type(msg.context_windows) == "table" then
    for model_id, ctx_size in pairs(msg.context_windows) do
      if type(ctx_size) == "number" and ctx_size > 0 then
        model_context_windows[model_id] = ctx_size
      end
    end
  end
  -- Update the open model_picker popup if one is up; otherwise drop.
  if not (state.popup and state.popup.variant == "model_picker") then
    return state, {}
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
  for _, prov in ipairs(state.popup.providers or {}) do
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
  local prev_awaiting = state.popup.awaiting or {}
  local new_awaiting = {}
  for k, v in pairs(prev_awaiting) do new_awaiting[k] = v end
  new_awaiting[provider] = nil
  return shallow_merge(state, {
    popup = shallow_merge(state.popup, {
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
    auth = auth, supports_login = supports, popup = new_popup,
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
  if state.popup and state.popup.variant == "tool_permission" then
    local queue = {}
    for _, q in ipairs(state.popup_queue or {}) do queue[#queue + 1] = q end
    queue[#queue + 1] = new_popup
    return shallow_merge(state, { popup_queue = queue }), {}
  end
  return shallow_merge(state, { popup = new_popup }), {}
end

local function handle_gate_mode_changed(msg, state)
  return shallow_merge(state, { gate_yolo = msg.mode == "yolo" }), {}
end

-- ── DAG observation ───────────────────────────────────────────────────

local function handle_graph_run_started(msg, state)
  if state.replay_mode then return state, {} end
  local now = tui.now_ms()
  return dag.run_started(state, msg.run_id or "", msg.total_nodes or 0, now), {}
end

local function handle_graph_node_fired(msg, state)
  if state.replay_mode then return state, {} end
  if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
  if (msg.firing_id or "") == "" then return state, {} end
  local now = tui.now_ms()
  local with_dispatch = dag.node_dispatched(state, msg.run_id, msg.node_id, msg.reasoner or "", now)
  local prev_map = with_dispatch.firing_to_node or {}
  local next_map = {}
  for k, v in pairs(prev_map) do next_map[k] = v end
  next_map[msg.firing_id] = { run_id = msg.run_id, node_id = msg.node_id }
  return shallow_merge(with_dispatch, { firing_to_node = next_map }), {}
end

local function handle_graph_node_tool_invoke(msg, state)
  if state.replay_mode then return state, {} end
  if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
  if type(msg.tool_name) ~= "string" or #msg.tool_name == 0 then return state, {} end
  local now = tui.now_ms()
  return dag.node_tool_invoked(state, msg.run_id, msg.node_id, msg.tool_name, msg.tool_args, now), {}
end

local function handle_graph_node_chat_bound(msg, state)
  if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
  if type(msg.chat_id) ~= "string" or #msg.chat_id == 0 then return state, {} end
  local prev = state.chat_id_to_node or {}
  local next_map = {}
  for k, v in pairs(prev) do next_map[k] = v end
  next_map[msg.chat_id] = { run_id = msg.run_id, node_id = msg.node_id }
  return shallow_merge(state, { chat_id_to_node = next_map }), {}
end

local function handle_tool_result(msg, state)
  if state.replay_mode then return state, {} end
  local id = msg.id
  if type(id) ~= "string" or id == "" then return state, {} end
  local now = tui.now_ms()
  -- Run-close: id matches a tracked run.
  if state.dag_runs and state.dag_runs[id] then
    local result = msg.result
    local status, results
    if type(result) == "table" then
      status  = result.status
      results = result.results
    end
    return dag.run_complete(state, id, status, results, now), {}
  end
  -- Per-firing close: look up firing_id → (run_id, node_id) map.
  local map_entry = (state.firing_to_node or {})[id]
  if map_entry then
    local run_id  = map_entry.run_id
    local node_id = map_entry.node_id
    local has_output = msg.result ~= nil
    local has_error  = msg.error  ~= nil
    local next_state = dag.node_result(state, run_id, node_id, has_output, has_error, now)
    local next_map = {}
    for k, v in pairs(state.firing_to_node or {}) do next_map[k] = v end
    next_map[id] = nil
    return shallow_merge(next_state, { firing_to_node = next_map }), {}
  end
  return state, {}
end

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
  ["key.ctrl_c"]                  = handle_exit,
  ["key.ctrl_d"]                  = handle_exit,
  ["key.ctrl_b"]                  = handle_toggle_sidebar,
  ["key.ctrl_o"]                  = handle_toggle_expand,
  ["key.?"]                       = handle_help_key,
  ["key.shift_?"]                 = handle_help_key,
  ["key.escape"]                  = handle_escape,
  ["sessions.session_end"]        = handle_session_end,
  ["sessions.session_start"]      = handle_session_start,
  ["sessions.replay.start"]       = handle_replay_start,
  ["sessions.replay.end"]         = handle_replay_end,
  ["chat.reset"]                  = handle_chat_reset,
  ["chat.message.append"]         = handle_message_append,
  ["chat.stream.delta"]           = handle_stream_delta,
  ["chat.stream.end"]             = handle_stream_end,
  ["chat.stream.reasoning_delta"] = handle_reasoning_delta,
  ["chat.stream.reasoning_end"]   = handle_reasoning_end,
  ["chat.session.stats"]          = handle_session_stats,
  ["chat.tool.start"]             = handle_tool_start,
  ["chat.tool.end"]               = handle_tool_end,
  ["chat.graph_result.append"]    = handle_graph_result_append,
  ["chat.plan.append"]            = handle_plan_append,
  ["lead-workflow.plan.approved"] = handle_plan_approved,
  ["chat.popup"]                  = handle_popup,
  ["chat.toast"]                  = handle_toast,
  ["chat.model.set_ack"]          = handle_model_set_ack,
  ["chat.models.listed"]          = handle_models_listed,
  ["chat.auth.status"]            = handle_auth_status,
  ["chat.tool.popup_request"]     = handle_tool_popup_request,
  ["tool-gate.mode_changed"]      = handle_gate_mode_changed,
  ["graph.run_started"]           = handle_graph_run_started,
  ["graph.node.fired"]            = handle_graph_node_fired,
  ["graph.node.tool.invoke"]      = handle_graph_node_tool_invoke,
  ["graph.node.chat.bound"]       = handle_graph_node_chat_bound,
  ["tool.result"]                 = handle_tool_result,
  ["mouse.selection"]             = handle_mouse_selection,
}

-- ── key / popup / scroll routing (fall-through for non-dispatched) ────

local function route_keys_and_popups(msg, state)
  local kind = msg.kind or ""

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

  -- Tool permission popup keys.
  if state.popup and state.popup.variant == "tool_permission" then
    if kind == "key.a" or kind == "key.A" or kind == "key.enter" then
      local id = state.popup.id
      return shallow_merge(state, pop_next_popup(state)), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "approve" } },
      }
    end
    if kind == "key.d" or kind == "key.D" then
      local id = state.popup.id
      return shallow_merge(state, pop_next_popup(state)), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "deny" } },
      }
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
        return shallow_merge(state, {
          popup = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending = false, dag_runs = {}, firing_to_node = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
          queued_entry_idx = NIL_SENTINEL,
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
    return state, {}
  end

  return state, {}
end

-- ── main entry point ──────────────────────────────────────────────────

function M.update(msg, state)
  state = prune_expired(state)
  local kind = msg.kind or ""
  log.log("update", "dispatch kind=%s", kind)
  local handler = handlers[kind]
  if handler then return handler(msg, state) end
  return route_keys_and_popups(msg, state)
end

return M
