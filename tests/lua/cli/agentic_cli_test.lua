-- Frontend contract tests: no provider, tty, stdin, or timing heuristic.
local subscriptions, sent, exits, timers = {}, {}, {}, {}
local out, err = {}, {}
local ready = false
local failures, interruptions = {}, {}
package.loaded["libs.agentic-loop"] = {
  is_ready = function() return ready end,
  fail_request = function(id, error) failures[#failures + 1] = { id = id, error = error } end,
  interrupt_request = function(id) interruptions[#interruptions + 1] = id end,
}
package.loaded["libs.sessions"] = { ready = function() return false end }
package.loaded["core.replay_window"] = { active = function() return false end }
nefor.bus.on_event = function(pattern, callback) subscriptions[#subscriptions + 1] = { pattern, callback } end
nefor.process = { spawn = function(spec) timers[#timers + 1] = spec; return #timers end }
nefor.engine.send = function(payload) sent[#sent + 1] = nefor.json.decode(payload) end
nefor.engine.shutdown = function(spec) exits[#exits + 1] = spec end
nefor.io.read_line = function() error("headless must never read stdin") end
local old_out, old_err = io.stdout, io.stderr
io.stdout = { write = function(_, s) out[#out + 1] = s end, flush = function() end }
io.stderr = { write = function(_, s) err[#err + 1] = s end, flush = function() end }
local cli = require("libs.cli")
local function emit(body)
  local entry = { payload = nefor.json.encode({ from = "tool-gate", body = body }) }
  for _, sub in ipairs(subscriptions) do if sub[1] == "*" or sub[1] == body.kind then sub[2](entry) end end
end
local function reset()
  subscriptions, sent, exits, timers, out, err, failures, interruptions = {}, {}, {}, {}, {}, {}, {}, {}
  ready = false
end
local function start(format)
  return cli.start { prompt = "question", format = format, readiness = {
    required_plugins = { "tool-gate" },
    required_provider = function() return "provider" end,
    required_tools = { "read_file" },
  } }
end
local function activate()
  emit { kind = "sessions.session_start", session_id = "s1", from_resume = true }
  emit { kind = "provider.hello" }
  emit { kind = "tool-gate.hello" }
  emit { kind = "tool.register", tools = { { name = "read_file" } } }
  assert(#sent == 0, "no submit during replay or before context ready")
  emit { kind = "sessions.replay.end" }
  assert(#sent == 0, "chunk end is not resume completion")
  emit { kind = "sessions.resume_done", session_id = "s1" }
  assert(#sent == 0, "resume does not bypass context commit")
  ready = true
  emit { kind = "agentic_loop.ready" }
  assert(#sent == 1 and sent[1].body.kind == "chat.input.submit")
  assert(sent[1].body.text == "question" and sent[1].body.submission_id)
end

do
  local id = start("json")
  activate()
  assert(sent[1].body.submission_id == id)
  emit { kind = "agentic_loop.idle" }
  emit { kind = "mag.run_result", status = "completed" }
  emit { kind = "agentic_loop.request_completed", request_id = "unrelated", status = "success" }
  assert(#out == 0 and #exits == 0, "no activity/count/turn heuristics")
  emit { kind = "agentic_loop.request_completed", request_id = id, session_id = "s1", status = "success", answer = "final" }
  assert(#exits == 0 and #out == 0, "wait for durable session flush")
  assert(sent[2].body.kind == "sessions.flush_request")
  emit { kind = "sessions.flush_done", request_id = id }
  local answer = nefor.json.decode(table.concat(out))
  assert(answer.session_id == "s1" and answer.request_id == id and answer.answer == "final")
  assert(exits[1].code == 0 and #exits == 1)
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "success", answer = "duplicate" }
  assert(#exits == 1)
  assert(table.concat(err) == "session_id: s1\n")
end

reset()
do
  local id = start("text")
  activate()
  emit { kind = "chat.tool.popup_request", id = "permission-1" }
  assert(sent[2].body.kind == "tool.permission_response" and sent[2].body.decision == "deny")
  assert(failures[1].id == id and failures[1].error.code == "approval_required")
  assert(#exits == 0, "approval failure must settle accepted work")
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "error", error = failures[1].error }
  emit { kind = "sessions.flush_done", request_id = id }
  assert(exits[1].code == 1 and #out == 0)
end
reset()
do
  local id = start("json")
  activate()
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "success", answer = "not durable" }
  emit { kind = "engine.plugin_process_terminated", plugin = "basic-tools" }
  assert(#exits == 0, "plugin death during flush must not bypass the barrier")
  emit { kind = "sessions.flush_done", request_id = id, error = { code = "persistence_failed", message = "disk full" } }
  assert(exits[1].code == 1)
  assert(nefor.json.decode(table.concat(out)).status == "error")
end
reset()
do
  local id = start("json")
  activate()
  emit { kind = "agentic_loop.request_completed", request_id = id,
    status = "success", answer = "canonical" }
  emit { kind = "engine.plugin_process_terminated", plugin = "basic-tools" }
  emit { kind = "engine.plugin_process_terminated", plugin = "tool-gate" }
  assert(#exits == 0 and #out == 0, "later plugin deaths preserve the flush barrier")
  emit { kind = "sessions.flush_done", request_id = id }
  local result = nefor.json.decode(table.concat(out))
  assert(result.status == "success" and result.answer == "canonical",
    "canonical request completion is immutable while flushing")
  assert(exits[1].code == 0)
end
reset()
do
  local id = start("json")
  activate()
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "interrupted" }
  emit { kind = "sessions.flush_done", request_id = id }
  assert(exits[1].code == 130)
end
reset()
do
  local id = start("json")
  activate()
  emit { kind = "engine.interrupt_requested", signal = "SIGINT" }
  emit { kind = "engine.interrupt_requested", signal = "SIGINT" }
  assert(interruptions[1] == id and #interruptions == 1)
  assert(#exits == 0 and #out == 0, "SIGINT waits for canonical request settlement")
  emit { kind = "mag.run_result", run_id = "lead-run", status = "killed" }
  assert(#exits == 0, "run terminal alone is not request completion")
  emit { kind = "agentic_loop.request_completed", request_id = id, session_id = "s1",
    status = "interrupted", error = { code = "interrupted", message = "request interrupted" } }
  assert(sent[#sent].body.kind == "sessions.flush_request" and #exits == 0)
  emit { kind = "sessions.flush_done", request_id = id }
  assert(exits[1].code == 130)
end
reset()
do
  local id = start("json")
  activate()
  emit { kind = "engine.plugin_process_terminated", plugin = "mock-plugin" }
  assert(#failures == 0 and #exits == 0, "optional plugin death must not cancel accepted work")
  emit { kind = "engine.plugin_process_terminated", plugin = "provider" }
  assert(failures[1].id == id and failures[1].error.code == "plugin_terminated")
  assert(#exits == 0 and #out == 0, "required plugin death waits for canonical request settlement")
  emit { kind = "agentic_loop.request_completed", request_id = id, session_id = "s1",
    status = "error", error = failures[1].error }
  assert(sent[#sent].body.kind == "sessions.flush_request" and #exits == 0)
  emit { kind = "sessions.flush_done", request_id = id }
  assert(exits[1].code == 1)
end
reset()
do
  local id = start("json")
  emit { kind = "sessions.flush_done", request_id = id }
  assert(#out == 0 and #exits == 0, "unsolicited flush completion cannot finish(nil)")
  activate()
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "success", answer = "not persisted" }
  emit { kind = "sessions.persistence_failed", message = "uncorrelated fault" }
  assert(#exits == 0, "uncorrelated persistence failure cannot bypass flush")
  emit { kind = "sessions.persistence_failed", request_id = id, message = "flush owner failed" }
  local result = nefor.json.decode(table.concat(out))
  assert(result.status == "error" and result.error.code == "session_failure")
  assert(#exits == 1 and exits[1].code == 1)
  emit { kind = "sessions.flush_done", request_id = id }
  assert(#exits == 1, "late flush acknowledgment is inert")
end
reset()
do
  local id = start("json")
  activate()
  local send = nefor.engine.send
  nefor.engine.send = function(payload)
    if nefor.json.decode(payload).body.kind == "sessions.flush_request" then error("writer unavailable") end
    return send(payload)
  end
  emit { kind = "agentic_loop.request_completed", request_id = id, status = "error", error = { message = "rejected" } }
  nefor.engine.send = send
  assert(#exits == 1 and exits[1].code == 1)
  assert(nefor.json.decode(table.concat(out)).error.code == "session_failure")
end
reset()
start("json")
timers[1].on_exit()
assert(#exits == 0 and sent[1].body.kind == "sessions.flush_request")
emit { kind = "sessions.flush_done", request_id = sent[1].body.request_id }
assert(exits[1].code == 1 and nefor.json.decode(table.concat(out)).error.code == "startup_failure")
reset()
cli.start { prompt = " ", readiness = {} }
assert(exits[1].code == 2 and #timers == 0)
io.stdout, io.stderr = old_out, old_err
print("agentic_cli_test: all assertions passed")
