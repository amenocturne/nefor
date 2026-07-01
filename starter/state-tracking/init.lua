-- state-tracking/init.lua — best-effort runtime state side effects.
--
-- This actor observes generic chat/session/runtime bus traffic, computes a
-- small Nefor runtime state once, then fans that state out to integrations:
-- clamor state publishing and desktop input-needed notifications.

local json = nefor.json

local sessions = require("sessions")

local CLAMOR_AGENT_ID = os.getenv("CLAMOR_AGENT_ID")
local NOTIFICATION_TITLE = "Nefor"
local INPUT_MESSAGE = "Waiting for your input"

local state = {
  session_token = nil,
  running = false,
  active_tool = nil,
  mode = nil,
  last_clamor = { mode = nil, session_token = nil, tool = nil },
  last_notification_mode = nil,
}

local function sh_quote(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function applescript_quote(s)
  return tostring(s):gsub("\\", "\\\\"):gsub('"', '\\"')
end

local function current_session_token()
  if type(state.session_token) == "string" and #state.session_token > 0 then
    return state.session_token
  end
  local id = sessions.current_id()
  if type(id) == "string" and #id > 0 then
    state.session_token = id
    return id
  end
  return nil
end

local function platform_name()
  local p = io.popen("uname -s 2>/dev/null")
  if not p then return "" end
  local name = p:read("*l") or ""
  p:close()
  return name
end

local function publish_clamor_state(mode, tool)
  if type(CLAMOR_AGENT_ID) ~= "string" or #CLAMOR_AGENT_ID == 0 then return end
  local token = current_session_token()
  if type(token) ~= "string" or #token == 0 then return end

  if mode == state.last_clamor.mode
      and token == state.last_clamor.session_token
      and tool == state.last_clamor.tool then
    return
  end
  state.last_clamor = { mode = mode, session_token = token, tool = tool }

  local parts = {
    "clamor", "set-state", sh_quote(mode),
    "--agent", sh_quote(CLAMOR_AGENT_ID),
    "--session-token", sh_quote(token),
  }
  if type(tool) == "string" and #tool > 0 then
    parts[#parts + 1] = "--tool"
    parts[#parts + 1] = sh_quote(tool)
  end

  -- Best effort only: clamor may not be installed, may have exited, or
  -- may reject this agent/session. Never let that affect Nefor.
  pcall(os.execute, table.concat(parts, " ") .. " >/dev/null 2>&1")
end

local function publish_input_notification()
  local platform = platform_name()

  if platform == "Darwin" then
    local script = 'display notification "' .. applescript_quote(INPUT_MESSAGE)
                .. '" with title "' .. applescript_quote(NOTIFICATION_TITLE) .. '"'
    local cmd = "/usr/bin/osascript -e " .. sh_quote(script) .. " 2>/dev/null & "
             .. "afplay /System/Library/Sounds/Tink.aiff 2>/dev/null &"
    pcall(os.execute, cmd)
  elseif platform == "Linux" then
    pcall(os.execute, "notify-send " .. sh_quote(NOTIFICATION_TITLE)
      .. " " .. sh_quote(INPUT_MESSAGE) .. " >/dev/null 2>&1 &")
  end
end

local function notify_if_input_transition(mode)
  if mode ~= "input" then
    state.last_notification_mode = mode
    return
  end
  if state.last_notification_mode == "input" then return end
  state.last_notification_mode = "input"
  publish_input_notification()
end

local function is_idle_runtime_state(body)
  if body.kind == "agentic_loop.idle" then return true end
  return body.kind == "agentic_loop.runtime_state" and body.state == "idle"
end

local function compute_runtime_state(body)
  local kind = body.kind

  if kind == "sessions.session_start" then
    if type(body.session_id) == "string" and #body.session_id > 0 then
      state.session_token = body.session_id
    end
    return nil
  end

  if kind == "chat.input.submit" or kind == "agentic_loop.run_start" then
    return { mode = "working", tool = state.active_tool }
  end

  if kind == "chat.tool.start" then
    local tool = body.name or body.tool
    if type(tool) == "string" and #tool > 0 then
      state.active_tool = tool
    end
    return { mode = "working", tool = state.active_tool }
  end

  if kind == "chat.tool.end" then
    state.active_tool = nil
    if state.running then
      return { mode = "working", tool = nil }
    end
    return nil
  end

  if is_idle_runtime_state(body) then
    return { mode = "input", tool = nil }
  end

  return nil
end

local function apply_runtime_state(runtime_state)
  if not runtime_state then return end

  state.mode = runtime_state.mode
  if runtime_state.mode == "working" then
    state.running = true
  elseif runtime_state.mode == "input" then
    state.running = false
    state.active_tool = nil
  end

  publish_clamor_state(runtime_state.mode, runtime_state.tool)
  notify_if_input_transition(runtime_state.mode)
end

local function handle_body(body)
  local runtime_state = compute_runtime_state(body)
  apply_runtime_state(runtime_state)
end

local function receive_msg(entry)
  if type(entry.payload) ~= "string" or entry.payload == "" then return end

  local ok, decoded = pcall(json.decode, entry.payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  handle_body(decoded.body)
end

return {
  name = "state-tracking",
  receive_msg = receive_msg,
  send_msg = function(_) end,

  _internals = {
    state = state,
    publish_clamor_state = publish_clamor_state,
    publish_input_notification = publish_input_notification,
    notify_if_input_transition = notify_if_input_transition,
    compute_runtime_state = compute_runtime_state,
    apply_runtime_state = apply_runtime_state,
    handle_body = handle_body,
    reset = function()
      state.session_token = nil
      state.running = false
      state.active_tool = nil
      state.mode = nil
      state.last_clamor = { mode = nil, session_token = nil, tool = nil }
      state.last_notification_mode = nil
    end,
  },
}
