-- A noninteractive frontend over the shared session and request lifecycle.
-- It never interprets tool names, graph activity, or turn terminals as completion.
local json_decode = require("core.json_decode")
local M = {}
local configured = nil
local function stderr(text) io.stderr:write(text); io.stderr:flush() end
local function stdout(text) io.stdout:write(text); io.stdout:flush() end

function M.start(opts)
  assert(type(opts) == "table" and type(opts.readiness) == "table", "cli.start requires readiness")
  local format = opts.format or "text"
  if type(opts.prompt) ~= "string" or not opts.prompt:find("%S")
      or (format ~= "text" and format ~= "json") then
    stderr("nefor: CLI requires a nonempty --prompt and --format text|json\n")
    nefor.engine.shutdown { code = 2, reason = "invalid CLI invocation", grace_ms = 2000 }
    return
  end
  local envelope = require("core.envelope")
  local loop = require("libs.agentic-loop")
  local sessions = require("libs.sessions")
  local replay = require("core.replay_window")
  local request_id = "request-" .. envelope.uuid_lite()
  local phase = "starting"
  local session_id, result
  local gate_mode = "safe"
  local settlement_requested = false

  local function required_plugin(name)
    local plugins = opts.readiness.required_plugins
    if type(plugins) == "function" then plugins = plugins() end
    for _, plugin in ipairs(plugins or {}) do
      if plugin == name then return true end
    end
    local provider = opts.readiness.required_provider
    if type(provider) == "function" then provider = provider() end
    return provider == name
  end

  local function finish(outcome)
    if phase == "finished" then return end
    phase = "finished"
    outcome.session_id = outcome.session_id or session_id
    outcome.request_id = request_id
    outcome.answer = outcome.answer or ""
    if outcome.error then
      stderr("nefor: " .. tostring(outcome.error.message or outcome.error.code or "request failed") .. "\n")
    end
    if format == "json" then
      stdout(nefor.json.encode(outcome) .. "\n")
    elseif outcome.status == "success" then
      stdout(outcome.answer .. "\n")
    end
    local code = outcome.status == "success" and 0 or (outcome.status == "interrupted" and 130 or 1)
    nefor.engine.shutdown { code = code, reason = "CLI request " .. outcome.status, grace_ms = 2000 }
  end

  local function flush(outcome)
    result = outcome
    phase = "flushing"
    local admitted, err = envelope.checked_emit_as("cli", nil,
      { kind = "sessions.flush_request", request_id = request_id })
    if not admitted then
      result.status = "error"
      result.error = { code = "session_failure",
        message = "Session flush request was not published: " .. tostring(err) }
      finish(result)
    end
  end

  local function fail(code, message)
    local err = { code = code, message = tostring(message) }
    if phase == "flushing" then
      if code ~= "session_failure" then return end
      result.status = "error"
      result.error = err
      finish(result)
    elseif phase == "submitted" then
      if settlement_requested then return end
      settlement_requested = true
      loop.fail_request(request_id, err)
    elseif phase ~= "flushing" then
      flush { status = "error", error = err }
    end
  end

  local function interrupt()
    if phase == "submitted" then
      if settlement_requested then return end
      settlement_requested = true
      loop.interrupt_request(request_id)
    elseif phase ~= "flushing" then
      finish { status = "interrupted", error = {
        code = "interrupted", message = "Request interrupted by runtime signal",
      } }
    end
  end

  nefor.bus.on_event("*", function(entry)
    if phase == "finished" or (entry.origin == "step" and entry.target ~= nil) then return end
    local ok, env = pcall(json_decode.owned, entry.payload or "")
    local body = ok and type(env) == "table" and env.body
    if type(body) ~= "table" or replay.active() then return end
    local kind = body.kind
    if kind == "sessions.session_start" and not body.from_resume
        or kind == "sessions.resume_done" then
      if session_id ~= body.session_id then
        session_id = body.session_id
        stderr("session_id: " .. tostring(session_id) .. "\n")
      end
    elseif kind == "engine.interrupt_requested" then
      interrupt()
    elseif kind == "engine.plugin_process_terminated" and required_plugin(body.plugin) then
      fail("plugin_terminated", "Required runtime process terminated: " .. tostring(body.plugin))
    elseif (kind == "sessions.transition_failed" and body.request_id == request_id)
        or (kind == "sessions.persistence_failed"
          and (phase ~= "flushing" or body.request_id == request_id)) then
      fail("session_failure", body.message)
    elseif kind == "tool-gate.mode_changed" then
      gate_mode = body.mode
    elseif phase == "submitted" and kind == "chat.tool.popup_request" then
      envelope.emit_as("cli", nil, {
        kind = "tool.permission_response", id = body.id, decision = "deny",
        reason = "Noninteractive frontend requires explicit approval",
      })
      fail("approval_required", "Tool requires interactive approval; no approval was granted")
    elseif phase == "submitted" and kind == "lead-workflow.plan.submitted" and gate_mode ~= "yolo" then
      envelope.emit_as("cli", nil, {
        kind = "chat.review.respond", text = "/reject Noninteractive frontend requires explicit approval",
      })
      fail("approval_required", "Review requires interactive approval; no approval was granted")
    elseif phase == "submitted" and kind == "agentic_loop.request_completed" and body.request_id == request_id then
      flush { session_id = body.session_id, request_id = request_id,
        status = body.status, answer = body.answer or "", error = body.error }
    elseif phase == "flushing" and kind == "sessions.flush_done" and body.request_id == request_id then
      if body.error then result.status = "error"; result.error = body.error end
      finish(result)
    elseif kind == "chat.error.append" and phase == "starting" then
      fail("startup_failure", body.message or body.title)
    end
  end)

  if nefor.events and nefor.events.on then
    nefor.events.on("shutdown", function()
      if phase == "submitted" then loop.interrupt_request(request_id) end
    end)
  end

  local readiness = {}
  for k, v in pairs(opts.readiness) do readiness[k] = v end
  readiness.is_ready = function()
    return session_id ~= nil and loop.is_ready()
  end
  readiness.on_ready = function()
    if phase ~= "starting" then return end
    phase = "submitted"
    envelope.emit_as("cli", nil, {
      kind = "chat.input.submit", text = opts.prompt, submission_id = request_id,
    })
  end
  readiness.on_error = function(message) fail("startup_failure", message) end
  require("libs.startup-readiness").wait(readiness)
  -- Supports a frontend attached after synchronous session initialization.
  if sessions.ready() and not session_id then
    session_id = sessions.current_id()
    stderr("session_id: " .. tostring(session_id) .. "\n")
  end
  return request_id
end

function M.configure(opts) configured = opts end

-- The legacy virtual-plugin spelling shares the same noninteractive path.
-- Only a single positional prompt is translated; there is no second parser/REPL.
function M.run(argv)
  local args = { "--frontend", "cli" }
  for _, arg in ipairs(argv or {}) do args[#args + 1] = arg end
  if #args == 3 and args[3]:sub(1, 1) ~= "-" then table.insert(args, 3, "--prompt") end
  local ok, opts = pcall(require("libs.startup").parse, args)
  if not ok then
    stderr("nefor: " .. tostring(opts) .. "\n")
    nefor.engine.shutdown { code = 2, reason = "invalid CLI invocation", grace_ms = 2000 }
    return 2
  end
  opts.readiness = assert(configured and configured.readiness, "CLI readiness not configured")
  local request_id = M.start(opts)
  assert(configured.initialize_session, "virtual CLI session initialization not configured")(opts.session_id, request_id)
  require("libs.startup").apply_mode(opts, require("libs.agentic-loop"))
  return 0
end
return M
