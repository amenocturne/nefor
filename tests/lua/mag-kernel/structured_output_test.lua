-- The intrinsic editor owns one draft per firing; only standalone submission
-- accepts a validated snapshot. All rejected calls remain ordinary receipts.
local factory = require("factories.structured-output")
local run_tool = require("factories.run-tool")
local files, serial, validations = {}, 0, 0
local real_schema = nefor.typed_json and nefor.typed_json.schema
local real_validate = nefor.typed_json and nefor.typed_json.validate
nefor.opaque_id = function() serial = serial + 1; return tostring(serial) end
nefor.fs = {
  data_root = function() return "/runtime-data" end,
  mkdir_p = function() return { ok = true } end,
  read_file = function(path)
    if files[path] == nil then return { ok = false, error = "not found" } end
    return { ok = true, content = files[path] }
  end,
  write_file_atomic = function(path, text)
    if text == "FAIL_IO" then return { ok = false, error = "disk unavailable" } end
    files[path] = text; return { ok = true }
  end,
}
nefor.typed_json = {
  schema = function(bound) return real_schema and real_schema(bound) or { type = "object", properties = { content = { type = "string" } } } end,
  validate = function(schema, text)
    validations = validations + 1
    if real_validate then return real_validate(schema, text) end
    local ok, value = pcall(nefor.json.decode, text)
    if not ok then return { ok = false, error = { code = "invalid_json", message = tostring(value) } } end
    if schema.root.kind == "list" then return { ok = true, value = value } end
    if type(value.content) ~= "string" then return { ok = false, violations = {
      { path = "$.content", code = "missing_field", expected = "string", actual = "missing", message = "required content" } } } end
    return { ok = true, value = value }
  end,
}
local schema = { version = 2, root = { kind = "record", fields = {
  { name = "content", schema = { kind = "string" } },
} } }
local function last(out, kind)
  for i = #out, 1, -1 do if out[i].kind == kind then return out[i] end end
end
local function count(out, kind)
  local n = 0; for _, v in ipairs(out) do if v.kind == kind then n = n + 1 end end; return n
end
local function make(dynamic, bound_schema, synchronous)
  local current_schema = bound_schema or schema
  local out, facts = {}, {}
  local actor, err
  actor, err = factory.construct("typed", {
    provider = "mock", tools = {}, schema = dynamic and { version = 2,
      root = { kind = "list", item = current_schema.root } } or current_schema,
    output_type = "output", error_type = "error", provider_error_type = "provider-error",
    dynamic = dynamic, dynamic_item_type = dynamic and "item" or nil,
    dynamic_item_descriptor = dynamic and {} or nil,
  }, function(v)
    out[#out + 1] = v
    if synchronous and v.kind == "generic-tool.ToolCalls" then
      local messages = {}
      for _, c in ipairs(v.calls) do
        messages[#messages + 1] = {role = "tool", tool_call_id = c.id, name = c.name,
          content = nefor.json.encode(c.runtime_result)}
      end
      actor.deliver({messages = {{message = {messages = messages}}}})
    end
  end, { conversation = {
    id = "conversation", turn_id = "turn", emit = function(v) facts[#facts + 1] = v end,
  } })
  assert(actor, err)
  actor.deliver({ messages = {{ message = { messages = {{ role = "user", content = "assignment" }} } }} })
  return actor, out, facts
end
local function reply(actor, out, calls, text)
  actor.deliver({ kind = "reply", ref = last(out, "capability.invoke").ref,
    result = { text = text, tool_calls = calls } })
end
local function call(name, args, id) return { id = id or name, name = name, args = args or {} } end
local function continue(actor, calls)
  local messages = {}
  for _, c in ipairs(calls) do messages[#messages + 1] = {
    role = "tool", tool_call_id = c.id, name = c.name, content = nefor.json.encode(c.runtime_result or {}) } end
  actor.deliver({ messages = {{ message = { messages = messages } }} })
end
local actor, out, facts = make()
local request = last(out, "capability.invoke").request
assert(request.output_schema == nil and #request.tools == 2 and #request.tool_specs == 2)
assert(request.tool_specs[1].parameters.properties.validate.default == false)
assert(request.tool_specs[1].parameters.properties.new_string.description:find("Whole draft", 1, true))
assert(request.tool_specs[2].description:find("sole tool call", 1, true))
local example
for _, f in ipairs(facts) do
  if f.kind == "content_chunk_appended" and f.chunk.kind == "text" then
    example = example or f.chunk.data:match('Example: write_output%((.-)%);')
  end
end
assert(example and nefor.json.decode(example).new_string == '{"count":1}')
-- Malformed native arguments quarantine the entire batch before any draft edit.
local original_request = last(out, "capability.invoke").ref
reply(actor, out, {call("write_output", {new_string = "must not write"}), {id = "bad", name = "submit_output", arguments = "["}})
assert(last(out, "generic-tool.ToolCalls") == nil and validations == 0)
-- Re-delivery of that stop cannot mutate the current firing or add a reminder.
local current_request = last(out, "capability.invoke").ref
local request_count = count(out, "capability.invoke")
actor.deliver({kind = "reply", ref = original_request, result = {text = "duplicate"}})
assert(last(out, "capability.invoke").ref == current_request and count(out, "capability.invoke") == request_count)
local before = validations
reply(actor, out, {call("write_output", {new_string = "{"})})
local calls = last(out, "generic-tool.ToolCalls").calls
local receipt = calls[1].runtime_result
local draft = receipt.draft
assert(receipt.write == "saved" and receipt.validation.status == "not_requested" and validations == before)
assert(files[draft] == "{" and files[calls[1].runtime_output_path] ~= nil)
continue(actor, calls)
reply(actor, out, {call("write_output", {new_string = "{", validate = true})})
calls = last(out, "generic-tool.ToolCalls").calls
assert(calls[1].runtime_result.write == "saved" and calls[1].runtime_result.validation.status == "invalid")
assert(files[draft] == "{" and count(out, "nefor.agent.Result") == 0)
continue(actor, calls)
reply(actor, out, {call("write_output", {new_string = '{"content":"kept"}', validate = true})})
calls = last(out, "generic-tool.ToolCalls").calls
assert(calls[1].runtime_result.validation.status == "valid" and count(out, "nefor.agent.Result") == 0)
continue(actor, calls)
for _, args in ipairs({
  {old_string = "missing", new_string = "replacement", validate = true},
  {old_string = "", new_string = "replacement", validate = true},
  {new_string = "FAIL_IO", validate = true},
}) do
  before = validations
  reply(actor, out, {call("write_output", args)})
  calls = last(out, "generic-tool.ToolCalls").calls
  assert(calls[1].runtime_result.write == "failed" and files[draft] == '{"content":"kept"}')
  assert(validations == before)
  continue(actor, calls)
end
for _, mixed in ipairs({
  {call("write_output", {old_string = "kept", new_string = "updated", validate = true}), call("submit_output"), call("read_file", {path = "ordinary"})},
  {call("submit_output"), call("write_output", {old_string = "updated", new_string = "kept"})},
}) do
  reply(actor, out, mixed)
  calls = last(out, "generic-tool.ToolCalls").calls
  local rejected
  for _, c in ipairs(calls) do if c.name == "submit_output" then rejected = c.runtime_result end end
  assert(rejected.submission == "rejected" and rejected.validation.error.code == "standalone_submission_required")
  assert(count(out, "nefor.agent.Result") == 0)
  local emitted = {}
  local executor = assert(run_tool.construct("executor", {tools = {"read_file"},
    tool_approval_policy = {rules = {}}}, function(v) emitted[#emitted + 1] = v end))
  executor.deliver({messages = {{message = {calls = calls}}}})
  if #calls == 3 then
    assert(count(emitted, "capability.invoke") == 1)
    local invoked = last(emitted, "capability.invoke")
    executor.deliver({kind = "reply", ref = invoked.ref, result = "normal result"})
  else assert(count(emitted, "capability.invoke") == 0) end
  local handle = last(emitted, "generic-tool.ToolHandle")
  assert(handle and #handle.results == #calls)
  continue(actor, calls)
end
reply(actor, out, {call("write_output", {new_string = '{"content":"aaa"}'})})
calls = last(out, "generic-tool.ToolCalls").calls; continue(actor, calls)
before = validations
reply(actor, out, {call("write_output", {old_string = "aa", new_string = "b", validate = true})})
calls = last(out, "generic-tool.ToolCalls").calls
assert(calls[1].runtime_result.error.code == "non_unique_match" and files[draft] == '{"content":"aaa"}' and validations == before)
continue(actor, calls)
-- Repeated final prose produces one reminder per stop and preserves prose.
for i = 1, 5 do reply(actor, out, nil, "investigation " .. i) end
assert(count(out, "nefor.agent.Result") == 0)
local reminders, investigations = 0, 0
for _, f in ipairs(facts) do
  if f.kind == "content_chunk_appended" then
    local text = f.chunk.data
    if type(text) == "string" and text:find("Your response ended", 1, true) then reminders = reminders + 1 end
    if type(text) == "string" and text:find("investigation", 1, true) then investigations = investigations + 1 end
  end
end
assert(reminders == 5 and investigations == 5)
-- Fresh validation reads the current draft, not a cached previous valid value.
files[draft] = "{}"
reply(actor, out, {call("submit_output")})
calls = last(out, "generic-tool.ToolCalls").calls
assert(calls[1].runtime_result.submission == "rejected" and calls[1].runtime_result.validation.status == "invalid" and count(out, "nefor.agent.Result") == 0)
continue(actor, calls)
files[draft] = '{"content":"accepted"}'
local requests = count(out, "capability.invoke")
reply(actor, out, {call("submit_output")})
assert(count(out, "capability.invoke") == requests and count(out, "nefor.agent.Result") == 1)
assert(last(out, "nefor.agent.Result").value.value.content == "accepted")
files[draft] = "{}"
assert(last(out, "nefor.agent.Result").value.value.content == "accepted")
-- A later firing has a fresh private draft.
actor.deliver({messages = {{message = {text = "next assignment"}}}})
reply(actor, out, {call("submit_output")})
calls = last(out, "generic-tool.ToolCalls").calls
assert(calls[1].runtime_result.draft ~= draft and calls[1].runtime_result.submission == "rejected" and calls[1].runtime_result.validation.status == "operational_error")
-- Dynamic values emit only after complete collection validation.
for _, value in ipairs({'[]', '[{"content":"first"},{"content":"second"}]'}) do
  local dynamic, dynamic_out = make(true)
  reply(dynamic, dynamic_out, {call("write_output", {new_string = value})})
  local writing = last(dynamic_out, "generic-tool.ToolCalls").calls
  continue(dynamic, writing)
  reply(dynamic, dynamic_out, {call("submit_output")})
  local outputs = {}; for _, v in ipairs(dynamic_out) do if v.kind == "nefor.agent.Result" then outputs[#outputs + 1] = v end end
  local expected = value == '[]' and 0 or 2
  assert(#outputs == expected + 1 and outputs[#outputs].dynamic.kind == "complete")
  assert(outputs[#outputs].dynamic.count == expected)
  if expected == 2 then assert(outputs[1].value.value.content == "first" and outputs[2].value.value.content == "second") end
end

-- Canonical products/scalars and a value-named record survive submission with
-- no provider-only root envelope. These cases protect the runtime wire codec.
if real_validate then
  for _, candidate in ipairs({
    {root = {kind = "int"}, text = "3"},
    {root = {kind = "product", components = {{kind = "string"}, {kind = "int"}}}, text = '["task",3]'},
    {root = {kind = "named", name = "test.Value", body = {kind = "record", fields = {{name = "value", schema = {kind = "int"}}}}}, text = '{"value":3}'},
  }) do
    local a, o = make(false, {version = 2, root = candidate.root})
    reply(a, o, {call("write_output", {new_string = candidate.text, validate = true})})
    local c = last(o, "generic-tool.ToolCalls").calls
    assert(c[1].runtime_result.validation.status == "valid")
    continue(a, c); reply(a, o, {call("submit_output")})
    assert(nefor.json.encode(last(o, "nefor.agent.Result").value.value) == candidate.text)
  end
end

-- Drain must settle pending recoverable responses instead of waiting forever
-- for a correction round that lifecycle policy disallows.
for _, pending_calls in ipairs({
  {{id = "bad", name = "write_output", arguments = "["}},
  {call("write_output", {new_string = "{}"})},
  {call("submit_output")},
}) do
  local a, o, f = make()
  a.handle_drain()
  reply(a, o, pending_calls)
  assert(count(o, "mag.failed") == 1 and count(o, "capability.invoke") == 1)
  local terminal = 0
  for _, fact in ipairs(f) do if fact.kind == "turn_failed" then terminal = terminal + 1 end end
  assert(terminal == 1)
end

-- Intrinsic receipts can return synchronously through the graph. Continuation
-- must be latched before emitting calls, so repair uses the same draft/firing.
do
  local a, o = make(false, nil, true)
  reply(a, o, {call("write_output", {new_string = "{"})})
  local original_draft = last(o, "generic-tool.ToolCalls").calls[1].runtime_result.draft
  reply(a, o, {call("write_output", {old_string = "{", new_string = '{"content":"synchronous"}', validate = true})})
  local result = last(o, "generic-tool.ToolCalls").calls[1].runtime_result
  assert(result.draft == original_draft and result.write == "saved" and result.validation.status == "valid")
  reply(a, o, {call("submit_output")})
  assert(count(o, "capability.invoke") == 3 and last(o, "nefor.agent.Result").value.value.content == "synchronous")
end
