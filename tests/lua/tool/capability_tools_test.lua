-- basic-tools and git-worktree tool definitions and their capability wrapper.
-- Driven by engine/tests/capability_tools_test.rs.

local json = nefor.json
local json_data = require("core.json_data")

local function eq(actual, expected, label)
  if actual ~= expected then
    error(string.format("%s:\n  expected %s\n  got      %s", label,
      tostring(expected), tostring(actual)), 2)
  end
end

-- Structural JSON equality, including the array/object distinction.
local function same(a, b, path)
  path = path or "$"
  if type(a) ~= "table" or type(b) ~= "table" then
    if a ~= b then error(path .. ": " .. tostring(a) .. " ~= " .. tostring(b), 2) end
    return
  end
  local a_array = json_data.is_array(a) or (next(a) ~= nil and #a > 0)
  local b_array = json_data.is_array(b) or (next(b) ~= nil and #b > 0)
  if a_array ~= b_array then error(path .. ": array/object mismatch", 2) end
  for key, value in pairs(a) do
    if b[key] == nil then error(path .. "." .. tostring(key) .. ": unexpected", 2) end
    same(value, b[key], path .. "." .. tostring(key))
  end
  for key in pairs(b) do
    if a[key] == nil then error(path .. "." .. tostring(key) .. ": missing", 2) end
  end
end

local function read_fixture(name)
  local file = assert(io.open(TOOL_FIXTURES .. "/" .. name, "r"))
  local text = file:read("a")
  file:close()
  return json.decode(text)
end

local sent, delivered = {}, {}
nefor.engine = {
  now = function() return 0 end,
  send = function(payload) sent[#sent + 1] = json.decode(payload) end,
  deliver = function(target, payload)
    delivered[#delivered + 1] = { target = target, envelope = json.decode(payload) }
  end,
}
package.preload["config"] = function()
  return { bin = function(name) return "/bin/" .. name end }
end

local tools = require("libs.compositors.tools")

local function last_sent(kind, id)
  for index = #sent, 1, -1 do
    local body = sent[index].body
    if body.kind == kind and (id == nil or body.id == id) then return body, sent[index].from end
  end
end

local function hello(spec)
  spec.to_plugin({ { type = "event", from = "tool-gate", body = { kind = "tool-gate.hello" }, replay = false } })
  -- Round-trip through JSON exactly as the gate would receive it.
  return json.decode(json.encode(last_sent("tool-gate.tools.advertise")))
end

local function invoke(spec, id, name, args)
  spec.to_plugin({ { type = "event", from = "tool-gate", replay = false, body = {
    kind = spec.name .. ".tool.invoke", id = id, name = name, args = args,
  } } })
end

-- Advertisements equal the previous Rust-owned advertisements.
local basic = tools.basic_actor_spec { max_read_bytes = 12345 }
eq(basic.name, "basic-tools", "actor keeps the basic-tools source name")
eq(table.concat(basic.command, " "), "/bin/basic-tools --read-file-max-bytes 12345",
  "binary receives the read cap and no gate flag")
same(hello(basic), read_fixture("basic-tools-advertisement.json"))
local worktree = tools.git_worktree_actor_spec()
eq(table.concat(worktree.command, " "), "/bin/git-worktree", "git-worktree has no gate flag")
same(hello(worktree), read_fixture("git-worktree-advertisement.json"))

-- Replayed hello and replayed invocations are inert.
local before = #sent
basic.to_plugin({ { type = "event", from = "tool-gate", body = { kind = "tool-gate.hello" }, replay = true } })
basic.to_plugin({ { type = "event", from = "tool-gate", replay = true, body = {
  kind = "basic-tools.tool.invoke", id = "replayed", name = "read_file", args = { path = "x" },
} } })
eq(#sent, before, "replayed traffic emits nothing")
eq(#delivered, 0, "replayed traffic delivers nothing")

-- Invalid arguments are rejected in Lua before any capability request.
invoke(basic, "bad-1", "read_file", { path = "x", max_bytes = 20000 })
eq(last_sent("tool.result", "bad-1").error,
  "read_file: argument `max_bytes` must be at most 12345, got 20000", "configured cap enforced in Lua")
invoke(basic, "bad-2", "write_file", { path = "x", new_string = "y", mode = "append" })
eq(last_sent("tool.result", "bad-2").error,
  "write_file: unknown argument `mode`; accepted arguments: path, new_string, old_string",
  "closed schema rejects unknown arguments")
invoke(basic, "bad-3", "read_image", { path = "x", extra = 1 })
eq(last_sent("tool.result", "bad-3").error,
  "read_image: unknown argument `extra`; accepted arguments: path, cwd",
  "schemas without additionalProperties are closed too")
invoke(worktree, "bad-4", "git_worktree_open", { repository = "/r", path = "/p" })
eq(last_sent("tool.result", "bad-4").error, "git_worktree_open: missing required argument `branch`",
  "git-worktree arguments validated in Lua")
eq(#delivered, 0, "no invalid invocation reaches a capability")

-- Valid invocations become capability requests; results settle the call.
invoke(basic, "gate-1", "read_file", { path = "README.md", offset = 2 })
local request = delivered[#delivered]
eq(request.target, "basic-tools", "capability request delivered to the binary")
eq(request.envelope.body.kind, "basic-tools.capability.invoke", "capability invoke kind")
eq(request.envelope.body.id, "gate-1", "capability request reuses the invocation id")
eq(request.envelope.body.capability, "read_file", "capability name")
eq(request.envelope.body.args.offset, 2, "validated arguments forwarded")
basic.from_plugin({ { type = "event", from = "basic-tools", body = {
  kind = "capability.result", id = "gate-1", output = "text",
} } })
local result, from = last_sent("tool.result", "gate-1")
eq(result.output, "text", "capability output becomes tool output")
eq(from, "basic-tools", "result emitted as the source")

invoke(basic, "gate-2", "write_file", { path = "x", new_string = "" })
basic.from_plugin({ { type = "event", from = "basic-tools", body = {
  kind = "capability.result", id = "gate-2", error = "io error",
} } })
eq(last_sent("tool.result", "gate-2").error, "io error", "capability error becomes tool error")

-- Process streams map to tool.stream; cancellation reaches the capability.
invoke(basic, "gate-3", "shell.script", { script = "sleep 9", cwd = ".",
  timeout = { present = false, milliseconds = 0 } })
basic.from_plugin({ { type = "event", from = "basic-tools", body = {
  kind = "capability.stream", id = "gate-3", stream = "stdout", text = "partial", skipped_bytes = 17,
} } })
local stream = last_sent("tool.stream", "gate-3")
eq(stream.text, "partial", "capability stream becomes tool.stream")
eq(stream.stream, "stdout", "stream channel preserved")
eq(stream.skipped_bytes, 17, "preview byte loss survives the capability wrapper")
basic.to_plugin({ { type = "event", from = "tool-gate", replay = false, body = {
  kind = "basic-tools.tool.cancel", id = "gate-3",
} } })
local cancel = delivered[#delivered].envelope.body
eq(cancel.kind, "basic-tools.capability.cancel", "cancel delivered to the binary")
eq(cancel.id, "gate-3", "cancel names the invocation")
local count = #sent
basic.from_plugin({ { type = "event", from = "basic-tools", body = {
  kind = "capability.result", id = "gate-3", error = "cancelled",
} } })
eq(#sent, count, "a cancelled invocation's late result is dropped")

-- git-worktree structured output passes through unchanged.
invoke(worktree, "gate-4", "git_worktree_open", { repository = "/r", path = "/p", branch = "b" })
eq(delivered[#delivered].envelope.body.kind, "git-worktree.capability.invoke", "worktree capability kind")
worktree.from_plugin({ { type = "event", from = "git-worktree", body = {
  kind = "capability.result", id = "gate-4", output = { ok = false, error = { kind = "git-failed" } },
} } })
eq(last_sent("tool.result", "gate-4").output.error.kind, "git-failed", "structured output preserved")

-- Other binary events are republished verbatim.
basic.from_plugin({ { type = "event", from = "basic-tools", body = { kind = "basic-tools.hello", version = "v" } } })
eq(last_sent("basic-tools.hello").version, "v", "hello republished")

-- Rejection at the new capability-to-tool boundary cancels process work and
-- settles the original invocation once; the binary's later result is inert.
local send = nefor.engine.send
nefor.engine.send = function(payload)
  local body = json.decode(payload).body
  if body.kind == "tool.stream" then error("frame_too_large") end
  return send(payload)
end
invoke(basic, "rejected-stream", "shell.script", { script = "sleep 9", cwd = ".",
  timeout = { present = false, milliseconds = 0 } })
basic.from_plugin({ { type = "event", body = {
  kind = "capability.stream", id = "rejected-stream", stream = "stdout", text = "rejected",
} } })
eq(delivered[#delivered].envelope.body.kind, "basic-tools.capability.cancel", "rejection cancels owned process")
eq(last_sent("tool.result", "rejected-stream").error_code, "output_rejected", "bounded owner failure")
eq(basic.source.cancel({ id = "rejected-stream" }), nil, "source settlement releases invocation ownership")
local settled_count = #sent
basic.from_plugin({ { type = "event", body = { kind = "capability.result", id = "rejected-stream", output = "late" } } })
eq(#sent, settled_count, "no duplicate terminal")
nefor.engine.send = send

local deliver = nefor.engine.deliver
nefor.engine.deliver = function() error("typed_admission", 0) end
nefor.engine.error_kind = function(fault) return fault == "typed_admission" and "frame_too_large" or nil end
invoke(basic, "rejected-invoke", "read_file", { path = "x" })
assert(last_sent("tool.result", "rejected-invoke").error:find("prior effects are unknown", 1, true))
settled_count = #sent
basic.from_plugin({ { type = "event", body = { kind = "capability.result", id = "rejected-invoke", output = "late" } } })
eq(#sent, settled_count, "rejected invocation releases capability waiter")
nefor.engine.deliver = deliver

return true
