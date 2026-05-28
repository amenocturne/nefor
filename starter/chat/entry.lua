local log = require("chat.log")

local M = {}

local version = 0

local function next_v()
  version = version + 1
  return version
end

local function copy(entry)
  local t = {}
  for k, v in pairs(entry) do t[k] = v end
  t.v = next_v()
  return t
end

-- constructors

function M.user(text)
  local v = next_v()
  log.log("entry", "create kind=text role=user v=%d", v)
  return { role = "user", kind = "text", text = text, v = v }
end

function M.system(text)
  local v = next_v()
  log.log("entry", "create kind=text role=system v=%d", v)
  return { role = "system", kind = "text", text = text, v = v }
end

function M.assistant(text)
  local v = next_v()
  log.log("entry", "create kind=text role=assistant v=%d", v)
  return { role = "assistant", kind = "text", text = text, v = v }
end

function M.assistant_stream()
  local v = next_v()
  log.log("entry", "create kind=stream role=assistant v=%d", v)
  return { role = "assistant", kind = "stream", text = "", streaming = true, v = v }
end

function M.tool_call(id, name, input, input_table)
  local v = next_v()
  log.log("entry", "create kind=tool_call name=%s v=%d", name or "?", v)
  return {
    role = "tool", kind = "tool_call",
    id = id, name = name, input = input, input_table = input_table,
    v = v,
  }
end

function M.graph_result(run_id, status, nodes, output, err, duration_ms)
  local v = next_v()
  log.log("entry", "create kind=graph_result run_id=%s v=%d", run_id or "?", v)
  return {
    role = "graph", kind = "graph_result",
    run_id = run_id, status = status, nodes = nodes,
    output = output, error = err, duration_ms = duration_ms,
    v = v,
  }
end

function M.plan(text, submitted_at)
  local v = next_v()
  log.log("entry", "create kind=plan v=%d", v)
  return {
    kind = "plan", text = text, submitted_at = submitted_at,
    status = "pending", v = v,
  }
end

function M.agents_md(path, dir, text)
  local v = next_v()
  log.log("entry", "create kind=agents_md path=%s v=%d", path or "?", v)
  return {
    kind = "agents_md", role = "system",
    path = path, dir = dir, text = text,
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

function M.set_output(entry, output, err_flag)
  local new = copy(entry)
  new.output = output
  new.error = err_flag
  log.log("entry", "mutate fn=set_output old_v=%d new_v=%d", entry.v, new.v)
  return new
end

function M.set_status(entry, status)
  local new = copy(entry)
  new.status = status
  log.log("entry", "mutate fn=set_status old_v=%d new_v=%d", entry.v, new.v)
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
