local log = require("libs.chat.log")

local M = {}

local version = 0
local local_identity = 0

local function next_v()
  version = version + 1
  return version
end

local function next_local_id()
  local_identity = local_identity + 1
  return "chat-local-entry-" .. tostring(local_identity)
end

local function copy(entry)
  local t = {}
  for k, v in pairs(entry) do t[k] = v end
  t.v = next_v()
  return t
end

-- constructors

function M.next_submission_id()
  return next_local_id()
end

function M.user(text, submission_id)
  local v = next_v()
  local local_id = next_local_id()
  log.log("entry", "create kind=text role=user v=%d", v)
  return {
    role = "user", kind = "text", text = text,
    local_id = local_id, submission_ids = { submission_id or local_id }, v = v,
  }
end

function M.system(text)
  local v = next_v()
  log.log("entry", "create kind=text role=system v=%d", v)
  return { role = "system", kind = "text", text = text, v = v }
end

function M.error(title, message, retryable)
  local v = next_v()
  log.log("entry", "create kind=error title=%s v=%d", title or "?", v)
  return {
    role = "system",
    kind = "error",
    title = title,
    message = message,
    retryable = retryable == true,
    v = v,
  }
end

function M.assistant_stream()
  local v = next_v()
  log.log("entry", "create kind=stream role=assistant v=%d", v)
  return { role = "assistant", kind = "stream", text = "", streaming = true, v = v }
end

function M.tool_call(id, name, input, input_table, display, raw_input, turn_id)
  local v = next_v()
  log.log("entry", "create kind=tool_call name=%s v=%d", name or "?", v)
  return {
    role = "tool", kind = "tool_call",
    id = id, exchange_id = id, turn_id = turn_id,
    name = name, input = input, input_table = input_table,
    raw_input = raw_input, display = display,
    v = v,
  }
end

function M.graph_result(run_id, status, nodes, output, err, duration_ms, run_name,
    invocation_label, invocation_kind)
  local v = next_v()
  log.log("entry", "create kind=graph_result run_id=%s v=%d", run_id or "?", v)
  return {
    role = "graph", kind = "graph_result",
    run_id = run_id, status = status, nodes = nodes,
    output = output, error = err, duration_ms = duration_ms,
    run_name = run_name, invocation_label = invocation_label,
    invocation_kind = invocation_kind,
    v = v,
  }
end

function M.plan(text, submitted_at, plan_id, status)
  local v = next_v()
  log.log("entry", "create kind=plan v=%d", v)
  return {
    kind = "plan", text = text, submitted_at = submitted_at, plan_id = plan_id,
    status = status or "pending", v = v,
  }
end

function M.agents_md(path, dir, text, notice_id)
  local v = next_v()
  log.log("entry", "create kind=agents_md path=%s v=%d", path or "?", v)
  return {
    kind = "agents_md", role = "system",
    path = path, dir = dir, text = text, notice_id = notice_id,
    v = v,
  }
end

function M.compaction(opts)
  opts = opts or {}
  local v = next_v()
  log.log("entry", "create kind=compaction request_id=%s v=%d", opts.request_id or "?", v)
  return {
    kind = "compaction", role = "system",
    request_id = opts.request_id,
    conversation_id = opts.conversation_id,
    provider = opts.provider,
    model = opts.model,
    strategy = opts.strategy,
    trigger = opts.trigger,
    status = opts.status or "complete",
    display_summary = opts.display_summary,
    model_context_artifact = opts.model_context_artifact,
    metadata = opts.metadata,
    v = v,
  }
end

-- mutations (copy-on-write, never mutate input)

function M.append_text(entry, delta)
  local new = copy(entry)
  new.text = (entry.text or "") .. delta
  log.log("entry", "mutate fn=append_text old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_text(entry, text)
  local new = copy(entry)
  new.text = text
  log.log("entry", "mutate fn=set_text old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_streaming(entry, streaming)
  local new = copy(entry)
  new.streaming = streaming
  log.log("entry", "mutate fn=set_streaming old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_model(entry, model)
  local new = copy(entry)
  new.model = model
  log.log("entry", "mutate fn=set_model old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_duration(entry, ms)
  local new = copy(entry)
  new.duration_ms = ms
  log.log("entry", "mutate fn=set_duration old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_turn_stats(entry, output_tokens, duration_ms)
  local new = copy(entry)
  if output_tokens ~= nil then new.output_tokens = output_tokens end
  if duration_ms ~= nil then new.duration_ms = duration_ms end
  return new
end

function M.set_turn_terminal(entry, terminal)
  terminal = terminal or {}
  local usage = type(terminal.usage) == "table" and terminal.usage or {}
  local new = M.set_turn_stats(entry,
    usage.output_tokens or usage.completion_tokens,
    terminal.duration_ms)
  if terminal.model ~= nil then new.model = terminal.model end
  return new
end

function M.set_output(entry, output, err_flag, completion_delivery)
  local new = copy(entry)
  new.output = output
  new.error = err_flag
  new.completion_delivery = completion_delivery
  log.log("entry", "mutate fn=set_output old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_status(entry, status)
  local new = copy(entry)
  new.status = status
  log.log("entry", "mutate fn=set_status old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.bind_canonical(entry, message_id, turn_id)
  local new = copy(entry)
  new.message_id = message_id
  new.turn_id = turn_id
  return new
end

function M.append_reasoning(entry, delta)
  local new = copy(entry)
  local prev = entry.reasoning or { text = "", streaming = true }
  new.reasoning = {
    text = (prev.text or "") .. delta,
    streaming = true,
    duration_ms = prev.duration_ms,
  }
  log.log("entry", "mutate fn=append_reasoning old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.finalize_reasoning(entry, duration_ms)
  local new = copy(entry)
  local prev = entry.reasoning or { text = "", streaming = true }
  new.reasoning = {
    text = prev.text,
    streaming = false,
    duration_ms = duration_ms or prev.duration_ms,
  }
  log.log("entry", "mutate fn=finalize_reasoning old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.finalize(entry, opts)
  local new = copy(entry)
  new.streaming = false
  if opts then
    if opts.model ~= nil then new.model = opts.model end
    if opts.duration_ms ~= nil then new.duration_ms = opts.duration_ms end
    if opts.text ~= nil then new.text = opts.text end
  end
  log.log("entry", "mutate fn=finalize old_v=%d new_v=%d", entry.v, new.v)
  return new
end

return M
