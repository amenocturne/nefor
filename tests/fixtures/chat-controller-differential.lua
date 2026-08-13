-- Differential contract for the controller extraction. The legacy reducer is a
-- frozen copy of examples/nefor-agent/chat/update.lua immediately before
-- 30b6eb06; it is test-only evidence, never placed on a production search path.
-- Reducers are called directly rather than through a rendered terminal. Replace
-- geometry/clipboard boundaries with deterministic no-ops so the comparison
-- remains pure and observes only returned state/effects.
tui.scroll_position = function()
  return { offset = 0, content_height = 0, viewport_height = 0, viewport_size = 10 }
end
tui.scroll_by = function() end
tui.scroll_to = function() end
tui.scroll_into_view = function() end
tui.scroll_reveal = function() end
tui.virtual_scroll_invalidate = function() end
tui.copy_to_clipboard = function() end

local legacy = assert(loadfile(assert(CHAT_PRE_EXTRACTION_ORACLE)))()
local controller = require("libs.chat.controller")
local dispatch = require("libs.chat.dispatch")
local commands = require("chat.commands")

local current = controller.build {
  handler_groups = {
    dispatch.group("starter commands", { ["input.submit"] = commands }),
  },
}

if CHAT_DIFFERENTIAL_MUTATION then
  local real = current
  current = function(msg, state)
    local next_state, effects = real(msg, state)
    if msg.kind == "tool-gate.mode_changed" then next_state.gate_mode = "mutated" end
    return next_state, effects
  end
end

local function copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local out = {}
  seen[value] = out
  for key, item in pairs(value) do out[copy(key, seen)] = copy(item, seen) end
  return out
end

local function initial_state()
  return {
    entries = {}, in_flight = nil, pending_graph_results = nil,
    conversation_id = nil,
    conversation_projection = require("libs.chat.conversation_projection").new(),
    input_value = "", show_sidebar = true, focus = "prompt", sidebar_cursor = 1,
    runs = {}, sidebar_folds = {}, scope_to_run = {}, node_previews = {},
    mag_arrivals = {}, capability_owners = {}, popup = nil, popup_queue = nil,
    stats = {}, current_context_tokens = nil, pending = false,
    turn_started_at = nil, last_turn_duration_ms = nil,
    model = "fixture-model", provider = "fixture-provider", mode = "default",
    reasoning_effort = "medium", max_tokens = nil, gate_mode = "safe",
    auth = { fixture_provider = "connected" }, supports_usage = {}, usage = {},
    expanded_details = false, raw_tool_id = nil, tool_displays = {},
    completion = nil, last_esc_ms = nil, escape_token = nil, escape_token_seq = 0,
    escape_count = nil, last_ctrl_c_ms = nil, exit_token = nil, exit_token_seq = 0,
    toasts = {}, prompt_history = {}, history_cursor = nil,
    command_completions = require("chat.slash").completions(),
  }
end

local function stable(value, seen)
  local kind = type(value)
  if kind ~= "table" then
    if kind == "string" then
      value = value:gsub("chat%-submission%-%d+", "<submission>")
      value = value:gsub("chat%-transition%-%d+", "<transition>")
      value = value:gsub("chat%-local%-entry%-%d+", "<local-entry>")
    end
    return kind .. ":" .. tostring(value)
  end
  seen = seen or {}
  if seen[value] then return "<cycle>" end
  seen[value] = true
  local keys = {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  local parts = {}
  for _, key in ipairs(keys) do
    -- Entry versions and generated optimistic IDs are representation noise;
    -- their surrounding canonical IDs, content, ordering, and effects remain.
    if key ~= "version" and key ~= "v" then
      parts[#parts + 1] = stable(key, seen) .. "=" .. stable(value[key], seen)
    end
  end
  seen[value] = nil
  return "{" .. table.concat(parts, ",") .. "}"
end

local legacy_state
local current_state
local step = 0
local function reset()
  legacy_state = initial_state()
  current_state = copy(legacy_state)
end

local function materialize(msg, state)
  local out = copy(msg)
  if out.request_id == "$pending" then
    out.request_id = state.pending_session_transition.request_id
  end
  return out
end

local function apply(label, msg)
  step = step + 1
  local legacy_msg = materialize(msg, legacy_state)
  local current_msg = materialize(msg, current_state)
  local next_legacy, legacy_effects = legacy.update(legacy_msg, legacy_state)
  local next_current, current_effects = current(current_msg, current_state)
  local legacy_state_view = stable(next_legacy)
  local current_state_view = stable(next_current)
  assert(legacy_state_view == current_state_view,
    string.format("differential state divergence at step %d (%s)\nlegacy: %s\ncurrent: %s",
      step, label, legacy_state_view, current_state_view))
  local legacy_effect_view = stable(legacy_effects or {})
  local current_effect_view = stable(current_effects or {})
  assert(legacy_effect_view == current_effect_view,
    string.format("differential effect divergence at step %d (%s)\nlegacy: %s\ncurrent: %s",
      step, label, legacy_effect_view, current_effect_view))
  legacy_state, current_state = next_legacy, next_current
end

local function sequence(name, events)
  reset()
  for index, event in ipairs(events) do apply(name .. "/" .. index, event) end
end

sequence("queue echo steering", {
  { kind = "chat.input.submit", _event_source = "startup", text = "queued", submission_id = "s1" },
  { kind = "chat.input.submit", _event_source = "startup", text = "steer", submission_id = "s2" },
  { kind = "chat.queue.steered" },
  { kind = "conversation.active.changed", conversation_id = "conversation-1" },
  { kind = "conversation.projection.delta", conversation_id = "conversation-1",
    change = { kind = "message_started", turn_id = "turn-1",
      message = { id = "user-1", turn_id = "turn-1", role = "user" } } },
  { kind = "conversation.projection.delta", conversation_id = "conversation-1",
    change = { kind = "message_completed", turn_id = "turn-1",
      message = { id = "user-1", turn_id = "turn-1", role = "user", text = "queued\nsteer",
        submission_ids = { "s1", "s2" } } } },
})

sequence("tool start end error", {
  { kind = "conversation.active.changed", conversation_id = "conversation-tools" },
  { kind = "conversation.projection.delta", conversation_id = "conversation-tools",
    change = { kind = "turn_started", turn_id = "turn-tools", run_id = "turn-tools" } },
  { kind = "conversation.projection.delta", conversation_id = "conversation-tools",
    change = { kind = "tool_call_completed", turn_id = "turn-tools",
      exchange = { id = "tool-ok", name = "read_file", status = "call_completed",
        arguments = { path = "README.md" } } } },
  { kind = "conversation.projection.delta", conversation_id = "conversation-tools",
    change = { kind = "tool_result_recorded", turn_id = "turn-tools",
      exchange = { id = "tool-ok", status = "result", result = "ok" } } },
  { kind = "conversation.projection.delta", conversation_id = "conversation-tools",
    change = { kind = "tool_call_completed", turn_id = "turn-tools",
      exchange = { id = "tool-bad", name = "bash", status = "call_completed", arguments = "false" } } },
  { kind = "conversation.projection.delta", conversation_id = "conversation-tools",
    change = { kind = "tool_error_recorded", turn_id = "turn-tools",
      exchange = { id = "tool-bad", status = "error", error = "failed" } } },
})

sequence("permission fifo responses", {
  { kind = "chat.tool.popup_request", id = "permission-1", tool = "write_file", args = { path = "a" } },
  { kind = "chat.tool.popup_request", id = "permission-2", tool = "bash", args = { command = "true" } },
  { kind = "key.a" },
  { kind = "key.d" },
})

sequence("safe auto yolo effects", {
  { kind = "input.submit", value = "/safe" },
  { kind = "tool-gate.mode_changed", mode = "normal" },
  { kind = "input.submit", value = "/auto" },
  { kind = "tool-gate.mode_changed", mode = "auto" },
  { kind = "input.submit", value = "/yolo" },
  { kind = "tool-gate.mode_changed", mode = "yolo" },
})

sequence("model think compact resume", {
  { kind = "input.submit", value = "/model model-b" },
  { kind = "chat.model.set_ack", provider = "fixture_provider", model = "model-b" },
  { kind = "input.submit", value = "/think high" },
  { kind = "chat.reasoning.set_ack", effort = "high" },
  { kind = "input.submit", value = "/compact" },
  { kind = "input.submit", value = "/resume session-b" },
  { kind = "sessions.session_start", session_id = "session-b", request_id = "$pending" },
  { kind = "sessions.resume_loading", session_id = "session-b", request_id = "$pending" },
})

sequence("plan controls", {
  { kind = "chat.plan.append", plan_id = "plan-1", submitted_at = 1, text = "Ship it" },
  { kind = "input.submit", value = "approve with note" },
  { kind = "lead-workflow.plan.approved", plan_id = "plan-1", approved = true },
})

sequence("matching stale replay ids", {
  { kind = "input.submit", value = "/resume session-current" },
  { kind = "sessions.session_start", session_id = "session-current", request_id = "stale-request" },
  { kind = "sessions.session_start", session_id = "session-current", request_id = "$pending" },
  { kind = "sessions.resume_loading", session_id = "session-current", request_id = "$pending" },
  { kind = "sessions.replay.start", session_id = "session-stale", request_id = "$pending", count = 9 },
  { kind = "sessions.replay.start", session_id = "session-current", request_id = "stale-request", count = 9 },
  { kind = "sessions.replay.start", session_id = "session-current", request_id = "$pending", count = 2 },
  { kind = "sessions.replay.progress", session_id = "session-current", request_id = "$pending", replayed = 1, total = 2 },
  { kind = "sessions.resume_done", session_id = "session-stale", request_id = "$pending" },
  { kind = "sessions.resume_done", session_id = "session-current", request_id = "$pending" },
})

sequence("mag lifecycle approval sidebar", {
  { kind = "mag.run_started", _event_source = "mag", run_id = "run-1", run_name = "fixture", scope = "scope-1", principal = "lead" },
  { kind = "mag.actor_spawned", run_id = "run-1", id = "actor-1", factory = "agent" },
  { kind = "mag.actor_ready", run_id = "run-1", id = "actor-1" },
  { kind = "mag.actor_busy", run_id = "run-1", id = "actor-1" },
  { kind = "mag.approval_request", run_id = "run-1", from = "gate-1", correlation = "approval-1", prompt = "Proceed?" },
  { kind = "key.a" },
  { kind = "key.tab" },
  { kind = "key.down" },
  { kind = "key.space" },
  { kind = "key.escape" },
  { kind = "mag.actor_idle", run_id = "run-1", id = "actor-1", completion = "done" },
  { kind = "mag.run_complete", run_id = "run-1" },
})

sequence("popup toast dismissal precedence", {
  { kind = "chat.toast", id = "toast-1", text = "notice", ttl_ms = 10000 },
  { kind = "chat.popup", level = "warning", title = "Heads up", message = "body" },
  { kind = "key.escape" },
  { kind = "key.escape" },
  { kind = "input.changed", value = "/mo" },
  { kind = "key.tab" },
  { kind = "key.escape" },
  { kind = "key.?" },
  { kind = "key.enter" },
})

tui.start {
  initial_state = { differential_steps = step },
  view = function(state)
    return tui.widget.text { content = tostring(state.differential_steps) }
  end,
  update = function(_msg, state) return state, {} end,
}
