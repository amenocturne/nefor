local tui_lib = require("nefor-tui")
local W = tui_lib.widget
local common = require("libs.chat.common")
local shallow_merge = common.shallow_merge
local NIL_SENTINEL = common.NIL_SENTINEL
local slash = require("chat.slash")
local sessions = require("libs.chat.sessions")
local at_path = require("libs.chat.at_path")
local history = require("libs.chat.history")
local transcript = require("libs.chat.transcript")
local usage_view = require("libs.chat.usage")
local quota_policy = require("libs.chat.quota_policy")
local ok_config, config = pcall(require, "config")
local usage_config = ok_config and config.active and config.active.usage or {
  command_ids = { "chatgpt/subscription", "openrouter/session-total" },
}
local Entry = require("libs.chat.entry")
local height_cache = require("libs.chat.height_cache")
local queued_input = require("libs.chat.queued_input")
local model_selection = require("libs.chat.model_selection")
local extensions = require("libs.chat.extensions")
local controller = require("libs.chat.controller")
local begin_session_transition = controller.lifecycle_context.begin_session_transition
local prepare_transition_effects = controller.lifecycle_context.prepare_transition_effects
local has_pending_plan = controller.lifecycle_context.has_pending_plan

return function(msg, state)
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
  local extension_state, extension_effects = extensions.handle_command(
    cmd, args, state, {
      patch = function(patch)
        return shallow_merge(state, patch or {})
      end,
      finish = function(patch)
        return shallow_merge(state, shallow_merge({
          input_value = "",
          completion = NIL_SENTINEL,
        }, patch or {}))
      end,
      new_session = function(patch)
        local pending, request_id = begin_session_transition(state, "new", patch)
        return pending, request_id
      end,
    })
  if extension_state ~= nil then
    local pending = extension_state.pending_session_transition
    if type(pending) == "table" then
      return prepare_transition_effects(
        extension_state, extension_effects, pending.request_id)
    end
    return extension_state, extension_effects
  end
  if cmd == "quit" or cmd == "exit" then
    return state, { { kind = "exit" } }
  end
  if cmd == "new" or cmd == "clear" then
    local pending, request_id = begin_session_transition(state, "new")
    return pending, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.interrupt_all" } },
      { kind = "send_to", target = "engine",
        body = { kind = "sessions.new_request", request_id = request_id } },
    }
  end
  if cmd == "help" then
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      popup = { variant = "help" },
    }), {}
  end
  if cmd == "usage" then
    if not quota_policy.surfaces_enabled(usage_config) then
      return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {}
    end
    local ids = {}
    for _, usage_id in ipairs(usage_config.command_ids or {}) do ids[#ids + 1] = usage_id end
    if #ids == 0 then
      return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL,
        popup = { variant = "warning", title = "/usage",
          body = "No configured provider exposes usage." } }), {}
    end
    local cached = {}
    for _, usage_id in ipairs(ids) do
      if type(state.usage) == "table" and type(state.usage[usage_id]) == "table"
          and state.usage[usage_id].kind ~= "unknown" then
        cached[#cached + 1] = { usage_id = usage_id, usage = state.usage[usage_id] }
      end
    end
    local usage_request_seq = (state.usage_request_seq or 0) + 1
    local request_id = "chat-usage:" .. tostring(usage_request_seq)
    return shallow_merge(state, {
      input_value = "", completion = NIL_SENTINEL,
      usage_request_seq = usage_request_seq,
      popup = {
        variant = "info",
        title = "usage",
        body = #cached > 0 and usage_view.markdown(cached) or "Fetching usage…",
        usage_request_id = request_id,
      },
    }), {
      { kind = "send_to", target = "engine",
        body = { kind = "conversation.usage.query", request_id = request_id,
          usage_ids = ids } },
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
    local cleared = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
    local resolution = model_selection.resolve(args, state.auth or {}, state.model_catalog)
    if resolution.kind == "select" then
      return model_selection.request(cleared, resolution.provider, resolution.model)
    end
    if resolution.kind == "ambiguous" then
      return shallow_merge(cleared, {
        popup = { variant = "warning", title = "/model",
          body = model_selection.ambiguous_message(resolution) },
      }), {}
    end
    if resolution.kind == "unresolved" then
      return shallow_merge(cleared, {
        popup = { variant = "warning", title = "/model",
          body = model_selection.unresolved_message(resolution) },
      }), {}
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
    }
    local next_state = transcript.push_entry(
      shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }),
      Entry.compaction({
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
      local pending, request_id = begin_session_transition(state, "resume", {
        resume_loading = { session_id = id, replayed = 0 },
        replay_mode = true,
      })
      return prepare_transition_effects(
        pending, { sessions.emit_resume_request(id) }, request_id)
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
    local submission_id = Entry.next_submission_id()
    local optimistic = queued_input.submit(state, wire_text, true, submission_id)
    local next_state = shallow_merge(optimistic, {
      input_value = "", completion = NIL_SENTINEL,
      prompt_history = hist, history_cursor = NIL_SENTINEL,
    })
    tui.scroll_into_view("transcript")
    return next_state, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = wire_text, submission_id = submission_id } },
    }
  end

  local submission_id = Entry.next_submission_id()
  local cleared = shallow_merge(queued_input.submit(state, wire_text, false, submission_id), {
    input_value = "", pending = true,
    turn_started_at = tui.now_ms(), completion = NIL_SENTINEL,
    prompt_history = hist,
    history_cursor = NIL_SENTINEL,
  })
  tui.scroll_into_view("transcript")
  return cleared, {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.input.submit", text = wire_text, submission_id = submission_id } },
  }
end
