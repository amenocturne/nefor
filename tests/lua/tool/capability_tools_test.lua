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
local classified = {}
local plan_approved = false
local basic = tools.basic_actor_spec {
  max_read_bytes = 12345,
  classify_command = function(policy, command)
    classified[#classified + 1] = policy .. " command " .. command
    if command:find("rm ", 1, true) then return "denied", "forbidden by the command classifier" end
    if command:find("ls", 1, true) then return "safe" end
    return "unsure", "not shown to be safe"
  end,
  classify_argv = function(policy, argv)
    classified[#classified + 1] = policy .. " argv " .. table.concat(argv, "|")
    if argv[1] == "ls" then return "safe" end
    return "unsure", "not shown to be safe"
  end,
  decide_write = function()
    if plan_approved then return "safe" end
    return "denied", "write_file requires an approved plan"
  end,
}
eq(pcall(tools.basic_actor_spec, { max_read_bytes = 1 }), false,
  "the command tools need the configured command classification")
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

-- What each tool says a call means under each approval policy. The gate asks
-- with a prepare request; nothing reaches the capability while it decides.
do
  local function prepare(spec, id, name, args, policy)
    spec.to_plugin({ { type = "event", from = "tool-gate", replay = false, body = {
      kind = spec.name .. ".tool.prepare", id = id, name = name, args = args, approval_policy = policy,
    } } })
    local prepared = last_sent("tool.prepared", id)
    assert(prepared, name .. " answers the prepare request under " .. policy)
    return prepared.decision, prepared.reason
  end
  local timeout = { present = false, milliseconds = 0 }
  local delivered_before = #delivered
  local sequence = 0
  local function check(spec, name, args, policy, decision, reason)
    sequence = sequence + 1
    local got, why = prepare(spec, "decide-" .. sequence, name, args, policy)
    eq(got, decision, name .. " under " .. policy)
    eq(why, reason, name .. " under " .. policy .. " gives its reason")
  end
  -- Reading tools: safe for a ReadOnly caller; Default is the gate's setting.
  for _, name in ipairs({ "read_file", "read_image" }) do
    check(basic, name, { path = "x" }, "ReadOnly", "safe", nil)
    check(basic, name, { path = "x" }, "Default", nil, nil)
  end
  check(worktree, "git_worktree_open", { repository = "/r", path = "/p", branch = "b" }, "ReadOnly", "safe", nil)
  check(worktree, "git_worktree_open", { repository = "/r", path = "/p", branch = "b" }, "Default", nil, nil)
  -- Writing tools: denied for a ReadOnly caller, with the reason.
  check(basic, "write_file", { path = "x", new_string = "" }, "ReadOnly", "denied", "it writes a file")
  check(worktree, "git_worktree_create", { repository = "/r", path = "/p", branch = "b", base = "main" },
    "ReadOnly", "denied", "it creates a Git worktree and a branch")
  check(worktree, "git_worktree_create", { repository = "/r", path = "/p", branch = "b", base = "main" },
    "Default", nil, nil)
  -- write_file under Default follows the composition's write decision.
  check(basic, "write_file", { path = "x", new_string = "" }, "Default", "denied",
    "write_file requires an approved plan")
  plan_approved = true
  check(basic, "write_file", { path = "x", new_string = "" }, "Default", "safe", nil)
  check(basic, "write_file", { path = "x", new_string = "" }, "ReadOnly", "denied", "it writes a file")
  -- Command tools: the configured classification, under every policy.
  classified = {}
  check(basic, "shell.script", { script = "ls -la", cwd = ".", timeout = timeout }, "ReadOnly", "safe", nil)
  check(basic, "shell.script", { script = "make", cwd = ".", timeout = timeout }, "Default", "unsure",
    "not shown to be safe")
  check(basic, "shell.script", { script = "rm -rf x", cwd = ".", timeout = timeout }, "Default", "denied",
    "forbidden by the command classifier")
  check(basic, "process.exec", { argv = { "ls", "-la" }, cwd = ".", timeout = timeout }, "ReadOnly", "safe", nil)
  check(basic, "process.exec", { argv = { "make", "all" }, cwd = ".", timeout = timeout }, "ReadOnly", "unsure",
    "not shown to be safe")
  eq(table.concat(classified, "; "),
    "ReadOnly command ls -la; Default command make; Default command rm -rf x; "
      .. "ReadOnly argv ls|-la; ReadOnly argv make|all",
    "each command tool asks the classifier with the caller's policy")
  eq(#delivered, delivered_before, "deciding sends nothing to a capability")

  -- A prepared call that is then invoked runs on the same id.
  prepare(basic, "prepared-run", "read_file", { path = "README.md" }, "ReadOnly")
  eq(#delivered, delivered_before, "still nothing before the invoke")
  basic.to_plugin({ { type = "event", from = "tool-gate", replay = false, body = {
    kind = "basic-tools.tool.invoke", id = "prepared-run", name = "read_file",
    args = { path = "README.md" }, approval_policy = "ReadOnly",
  } } })
  eq(delivered[#delivered].envelope.body.id, "prepared-run", "the invoke reaches the capability")
  basic.from_plugin({ { type = "event", from = "basic-tools", body = {
    kind = "capability.result", id = "prepared-run", output = "text",
  } } })
  -- Cancelling a prepared call that never ran sends no capability cancel.
  prepare(basic, "prepared-dropped", "shell.script", { script = "ls", cwd = ".", timeout = timeout }, "Default")
  local before_cancel = #delivered
  basic.to_plugin({ { type = "event", from = "tool-gate", replay = false, body = {
    kind = "basic-tools.tool.cancel", id = "prepared-dropped",
  } } })
  eq(#delivered, before_cancel, "a call that never executed has nothing to cancel in the binary")
end

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

-- Cancellation rejection is authority loss, even when source.cancel catches
-- handler errors. Stream rejection must still settle its actual invocation.
local losses = {}
local old_loop = package.loaded["libs.agentic-loop"]
package.loaded["libs.agentic-loop"] = {
  capability_lost = function(peer, message)
    losses[#losses + 1] = { peer = peer, message = message }
  end,
}
local function result_count(id)
  local total = 0
  for _, env in ipairs(sent) do
    if env.body.kind == "tool.result" and env.body.id == id then total = total + 1 end
  end
  return total
end
for _, stream_failure in ipairs({ false, true }) do
  local id = stream_failure and "rejected-stream-cancel" or "rejected-cancel"
  invoke(basic, id, "shell.script", { script = "sleep 9", cwd = ".",
    timeout = { present = false, milliseconds = 0 } })
  nefor.engine.deliver = function(_, payload)
    if json.decode(payload).body.kind == "basic-tools.capability.cancel" then
      error("typed_admission", 0)
    end
    return deliver("basic-tools", payload)
  end
  local before_losses = #losses
  if stream_failure then
    nefor.engine.send = function(payload)
      if json.decode(payload).body.kind == "tool.stream" then error("frame_too_large") end
      return send(payload)
    end
    basic.from_plugin({ { body = {
      kind = "capability.stream", id = id, stream = "stdout", text = "rejected",
    } } })
    eq(last_sent("tool.result", id).error_code, "output_rejected", "stream failure still settles actual id")
    eq(result_count(id), 1, "one physical stream-failure terminal")
  else
    basic.source.cancel({ id = id })
    eq(result_count(id), 0, "cancel authority loss is not fabricated completion")
  end
  eq(#losses, before_losses + 1, "cancel rejection reconciles once through surviving authority")
  eq(losses[#losses].peer, "basic-tools", "lost capability identity")
  assert(losses[#losses].message:find("outcomes are unknown", 1, true))
  eq(basic.source.cancel({ id = id }), nil, "invocation ownership released")
  local before_late = #sent
  basic.from_plugin({ { body = { kind = "capability.result", id = id, output = "late" } } })
  eq(#sent, before_late, "late result cannot duplicate settlement after cancel rejection")
  nefor.engine.deliver, nefor.engine.send = deliver, send
end
package.loaded["libs.agentic-loop"] = old_loop

return true
