-- Tool definition, advertisement, invocation, and settlement. Driven by
-- engine/tests/tool_constructor_test.rs.

local tool = require("libs.tool")
local json = nefor.json

local function eq(actual, expected, label)
  if actual ~= expected then
    error(string.format("%s:\n  expected %s\n  got      %s", label,
      tostring(expected), tostring(actual)), 2)
  end
end

local sent = {}
nefor.engine = {
  now = function() return 0 end,
  send = function(payload)
    local decoded = json.decode(payload)
    sent[#sent + 1] = decoded
  end,
}

local function last_result(id)
  for index = #sent, 1, -1 do
    local body = sent[index].body
    if body.kind == "tool.result" and body.id == id then return body, sent[index].from end
  end
end

local function display(label)
  return {
    compact = { label = label },
    expanded = { label = label, fields = {} },
    result = { kind = "content", fields = {} },
  }
end

local function spec(overrides)
  local value = {
    name = "echo",
    description = "Echo text.",
    parameters = {
      type = "object",
      properties = { text = { type = "string" }, times = { type = "integer", minimum = 1 } },
      required = { "text" },
    },
    display = display("echo"),
    handler = function(args, call) call.ok(args.text) end,
  }
  for key, item in pairs(overrides or {}) do value[key] = item end
  return value
end

local function define_fails(overrides, fragment)
  local ok, err = pcall(tool.define, spec(overrides))
  assert(not ok, "definition should fail: " .. fragment)
  assert(tostring(err):find(fragment, 1, true), "error mentions `" .. fragment .. "`: " .. tostring(err))
end

-- Definition-time checks.
define_fails({ name = "" }, "name must be a non-empty string")
define_fails({ access = "read" }, "unknown field `access`")
define_fails({ description = 3 }, "description must be a string")
define_fails({ parameters = { type = "object", properties = { x = { type = "string", format = "uri" } } } },
  "unsupported keyword `format`")
define_fails({ display = { compact = { label = "x" } } }, "invalid display")
define_fails({ handler = "nope" }, "handler must be a function")
define_fails({ cancel = 1 }, "cancel must be a function")

-- Advertisement.
local echo = tool.define(spec({ context = { folders = {} }, execution = { kind = "routed" } }))
local source = tool.source { name = "echo-source", tools = { echo } }
source.advertise("gate")
local advert = sent[#sent]
eq(advert.from, "echo-source", "advertisement is emitted as the source")
eq(advert.body.kind, "gate.tools.advertise", "advertisement targets the gate")
eq(advert.body.source, "echo-source", "advertisement names its source")
eq(#advert.body.tools, 1, "one tool advertised")
local advertised = advert.body.tools[1]
eq(advertised.name, "echo", "advertised name")
eq(advertised.description, "Echo text.", "advertised description")
eq(advertised.parameters.required[1], "text", "advertised schema unchanged")
eq(advertised.execution.kind, "routed", "execution metadata advertised")
assert(advertised.display.compact.label == "echo", "display advertised")
local encoded = json.encode(tool.define(spec({
  parameters = { type = "object", properties = {}, required = {} },
})).spec().parameters)
assert(encoded:find('"required":[]', 1, true), "empty required encodes as a JSON array: " .. encoded)
assert(encoded:find('"properties":{}', 1, true), "empty properties encodes as a JSON object: " .. encoded)
assert(not pcall(tool.source, { name = "dupe", tools = { echo, echo } }), "duplicate tool names fail")

-- Per-request descriptions render only for request-local specs.
local local_tool = tool.define(spec({
  name = "describe",
  description = function(request) return "schema: " .. request.schema end,
}))
eq(local_tool.spec({ schema = "S" }).description, "schema: S", "per-request description")
assert(not pcall(tool.source, { name = "x", tools = { local_tool } }),
  "advertised sources need a static description")

-- Invocation: valid arguments reach the handler; invalid ones never do.
assert(source.invoke({ kind = "echo-source.tool.invoke", id = "1", name = "echo", args = { text = "hi" } }))
local result, from = last_result("1")
eq(result.output, "hi", "handler settles with output")
eq(from, "echo-source", "result is emitted as the source")

local handled = 0
local counted = tool.source { name = "counted", tools = { tool.define(spec({
  handler = function(_, call) handled = handled + 1; call.ok("ran") end,
})) } }
counted.invoke({ id = "2", name = "echo", args = { text = "hi", extra = true } })
eq(last_result("2").error, "echo: unknown argument `extra`; accepted arguments: text, times",
  "unknown argument rejected uniformly")
counted.invoke({ id = "3", name = "echo", args = {} })
eq(last_result("3").error, "echo: missing required argument `text`", "missing argument rejected")
counted.invoke({ id = "4", name = "echo", args = { text = "x", times = 0 } })
eq(last_result("4").error, "echo: argument `times` must be at least 1, got 0", "range rejected")
counted.invoke({ id = "5", name = "echo" })
eq(last_result("5").error, "echo: missing required argument `text`", "absent args validate as {}")
eq(handled, 0, "invalid arguments never reach the handler")
counted.invoke({ id = "6", name = "missing", args = {} })
eq(last_result("6").error, "unknown tool `missing`; available tools: echo", "unknown tool")

-- A call naming a tool the caller cannot use: one message, listing what it can.
local ran
local function named(tool_name)
  return tool.define(spec({ name = tool_name, handler = function(_, call) ran = call.name; call.ok("ran") end }))
end
local several = tool.source { name = "several", tools = { named("write"), named("read"), named("list") } }
several.invoke({ id = "u1", name = "bash", args = {} })
eq(last_result("u1").error, "unknown tool `bash`; available tools: write, read, list",
  "without an allowlist every tool is listed in advertised order")
eq(tool.unknown_tool_message("bash", { "write", "read", "list" }), last_result("u1").error,
  "the source words the message through the shared function")
-- `shell` is allowlisted but not defined here; `write` is defined but excluded.
local allowlist = { "list", "shell", "read", "list" }
several.invoke({ id = "u2", name = "bash", args = {}, allowlist = allowlist })
eq(last_result("u2").error, "unknown tool `bash`; available tools: list, read",
  "an allowlist narrows and orders the list")
ran = nil
several.invoke({ id = "u3", name = "write", args = { text = "x" }, allowlist = allowlist })
eq(last_result("u3").error, "unknown tool `write`; available tools: list, read",
  "an excluded tool is answered like a nonexistent one")
eq(ran, nil, "an excluded tool never reaches its handler")
several.invoke({ id = "u4", name = "read", args = { text = "x" }, allowlist = allowlist })
eq(last_result("u4").output, "ran", "an allowlisted tool runs")
several.invoke({ id = "u5", name = "read", args = {}, allowlist = {} })
eq(last_result("u5").error, "unknown tool `read`; no tools are available", "empty allowlist")
eq(tool.unknown_tool_message("bash", {}), "unknown tool `bash`; no tools are available",
  "nothing available")

-- Handler failure settles exactly once.
local raising = tool.source { name = "raising", tools = { tool.define(spec({
  handler = function() error("boom", 0) end,
})) } }
raising.invoke({ id = "7", name = "echo", args = { text = "x" } })
eq(last_result("7").error, "echo: handler raised: boom", "raising handler settles with an error")

-- Deferred settlement, result metadata, streaming, and cancellation.
local deferred_call
local cancelled
local deferred = tool.source { name = "deferred", tools = { tool.define(spec({
  handler = function(_, call) deferred_call = call end,
  cancel = function(call) cancelled = call.id end,
})) } }
deferred.invoke({ id = "8", name = "echo", args = { text = "x" }, invocation = { run_id = "r" } })
eq(last_result("8"), nil, "deferred handler emits nothing yet")
eq(deferred_call.request.invocation.run_id, "r", "handler sees invocation metadata")
assert(not deferred_call.settled(), "pending call is unsettled")
deferred_call.stream("stdout", "chunk")
eq(sent[#sent].body.kind, "tool.stream", "stream emits tool.stream")
eq(sent[#sent].body.text, "chunk", "stream carries its text")
deferred.result("8", { output = { ok = true }, completion_delivery = "async" })
eq(last_result("8").completion_delivery, "async", "result metadata rides on the envelope")
assert(deferred_call.settled(), "result settles the call")

deferred.invoke({ id = "9", name = "echo", args = { text = "x" } })
assert(deferred.receive({ kind = "deferred.tool.cancel", id = "9" }), "cancel is routed")
eq(cancelled, "9", "tool cancel hook runs for the pending call")
assert(not deferred.receive({ kind = "deferred.tool.cancel", id = "9" }), "a settled call is not cancelled twice")

-- Pure-Lua actor: advertises on gate hello and serves invocations.
sent = {}
local actor = tool.actor { source = source, gate = "gate" }
actor.receive_msg({ origin = "plugin", payload = json.encode({ from = "gate", body = { kind = "gate.hello" } }) })
eq(sent[#sent].body.kind, "gate.tools.advertise", "actor advertises on gate hello")
actor.receive_msg({ origin = "step", payload = json.encode({ from = "gate", body = {
  kind = "echo-source.tool.invoke", id = "10", name = "echo", args = { text = "actor" },
} }) })
eq(last_result("10").output, "actor", "actor serves invocations")

return true
