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
    execute = function(args, call) call.ok(args.text) end,
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
define_fails({ execute = "nope" }, "execute must be a function")
define_fails({ execute = false }, "execute must be a function")
define_fails({ handler = function() end }, "unknown field `handler`")
define_fails({ prepare = "nope" }, "prepare must be a function or nil")
define_fails({ approval = "safe" }, "approval must be a table")
define_fails({ approval = { rules = {} } }, "approval has unknown field `rules`")
define_fails({ approval = { decide = "safe" } }, "approval.decide must be a function")
define_fails({ approval = { describe = { Rules = "x" } } }, "unknown approval policy `Rules`")
define_fails({ approval = { describe = { ReadOnly = "" } } }, "approval.describe.ReadOnly must be a non-empty string")
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

-- Invocation: valid arguments reach execute; invalid ones never do.
assert(source.invoke({ kind = "echo-source.tool.invoke", id = "1", name = "echo", args = { text = "hi" } }))
local result, from = last_result("1")
eq(result.output, "hi", "execute settles with output")
eq(from, "echo-source", "result is emitted as the source")

local handled = 0
local counted = tool.source { name = "counted", tools = { tool.define(spec({
  execute = function(_, call) handled = handled + 1; call.ok("ran") end,
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
eq(handled, 0, "invalid arguments never reach execute")
counted.invoke({ id = "6", name = "missing", args = {} })
eq(last_result("6").error, "unknown tool `missing`; available tools: echo", "unknown tool")

-- A call naming a tool the caller cannot use: one message, listing what it can.
local ran
local function named(tool_name)
  return tool.define(spec({ name = tool_name, execute = function(_, call) ran = call.name; call.ok("ran") end }))
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
eq(ran, nil, "an excluded tool never reaches execute")
several.invoke({ id = "u4", name = "read", args = { text = "x" }, allowlist = allowlist })
eq(last_result("u4").output, "ran", "an allowlisted tool runs")
several.invoke({ id = "u5", name = "read", args = {}, allowlist = {} })
eq(last_result("u5").error, "unknown tool `read`; no tools are available", "empty allowlist")
eq(tool.unknown_tool_message("bash", {}), "unknown tool `bash`; no tools are available",
  "nothing available")

-- A raising execute settles exactly once.
local raising = tool.source { name = "raising", tools = { tool.define(spec({
  execute = function() error("boom", 0) end,
})) } }
raising.invoke({ id = "7", name = "echo", args = { text = "x" } })
eq(last_result("7").error, "echo: execute raised: boom", "a raising execute settles with an error")

-- Deferred settlement, result metadata, streaming, and cancellation.
local deferred_call
local cancelled
local deferred = tool.source { name = "deferred", tools = { tool.define(spec({
  execute = function(_, call) deferred_call = call end,
  cancel = function(call) cancelled = call.id end,
})) } }
deferred.invoke({ id = "8", name = "echo", args = { text = "x" }, invocation = { run_id = "r" } })
eq(last_result("8"), nil, "a deferred execute emits nothing yet")
eq(deferred_call.request.invocation.run_id, "r", "execute sees invocation metadata")
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

-- The interface: every definition has execute, and prepare is nil unless given.
do
  local plain = tool.define(spec())
  eq(type(plain.execute), "function", "execute is part of every definition")
  eq(plain.prepare, nil, "a tool without a prepare step has prepare = nil")
  eq(tool.define(spec({ prepare = nil })).prepare, nil, "prepare = nil is the same as leaving it out")
  local prepared = tool.define(spec({ prepare = function() end }))
  eq(type(prepared.prepare), "function", "a declared prepare is kept")
end

-- Per-policy descriptions are advertised beside the description, never merged
-- into it here: appending is done once, where a request's tools are assembled.
do
  local described = tool.define(spec({ approval = {
    describe = { ReadOnly = "Rejected for you." },
  } }))
  eq(described.describe("ReadOnly"), "Rejected for you.", "the policy with text has it")
  eq(described.describe("Default"), nil, "a policy with nothing to add has nil")
  eq(described.spec().description, "Echo text.", "the description itself is unchanged")
  eq(described.spec().policy_descriptions.ReadOnly, "Rejected for you.", "advertised per policy")
  eq(described.spec().policy_descriptions.Default, nil, "nothing advertised for Default")
  local plain = tool.define(spec())
  eq(plain.describe("ReadOnly"), nil, "a tool without descriptions adds nothing")
  eq(plain.spec().policy_descriptions, nil, "and advertises none")
  eq(tool.define(spec({ approval = { describe = {} } })).spec().policy_descriptions, nil,
    "an empty table is the same as none")
end

-- Appending happens once, where one request's tools are assembled for an
-- agent: the registered specs, the names the agent may call, and its policy.
do
  local registered = {
    write = { name = "write", owner = "files", description = "Writes.", parameters = { type = "object" },
      execution = { kind = "routed" }, policy_descriptions = { ReadOnly = "Rejected for you." } },
    read = { name = "read", owner = "files", description = "Reads.", parameters = { type = "object" } },
    hidden = { name = "hidden", owner = "files", description = "Not allowed.",
      policy_descriptions = { ReadOnly = "Never shown." } },
  }
  local names = { "read", "write", "missing" }
  local specs = tool.request_tool_specs(names, registered, nil, "ReadOnly")
  eq(#specs, 1, "only a tool with text for the policy gets a request definition")
  eq(specs[1].name, "write", "the tool with text")
  eq(specs[1].description, "Writes.\n\nRejected for you.", "the text is appended to the description")
  eq(specs[1].owner, "files", "the definition keeps the registered owner")
  eq(specs[1].execution.kind, "routed", "and its execution")
  eq(registered.write.description, "Writes.", "the registered spec is not changed")
  eq(#tool.request_tool_specs(names, registered, nil, "Default"), 0, "nothing is appended for a policy without text")
  eq(#tool.request_tool_specs(names, registered, nil, nil), 0, "nor for a request without a policy")
  eq(#tool.request_tool_specs(names, registered, nil, "Rules"), 0, "nor for an unknown policy")
  eq(#tool.request_tool_specs({ "read" }, registered, nil, "ReadOnly"), 0,
    "a tool outside the agent's names is never described")
  local own = { { name = "write_output", description = "Own." }, { name = "write", description = "Request-local." } }
  local merged = tool.request_tool_specs(names, registered, own, "ReadOnly")
  eq(#merged, 2, "the request's own definitions are kept")
  eq(merged[2].description, "Request-local.", "and one of them is never replaced")
end

local function last_prepared(id)
  for index = #sent, 1, -1 do
    local body = sent[index].body
    if body.kind == "tool.prepared" and body.id == id then return body end
  end
end

-- prepare, then the decision function, then execute — on one call.
do
  local order = {}
  local seen = {}
  local phased = tool.source { name = "phased", tools = { tool.define(spec({
    prepare = function(args, call)
      order[#order + 1] = "prepare"
      seen.prepare_phase = call.phase
      call.note = "kept:" .. args.text
    end,
    approval = { decide = function(policy, args, call)
      order[#order + 1] = "decide"
      seen.policy, seen.decide_note, seen.call_policy = policy, call.note, call.policy
      if args.text == "bad" then return "denied", "it is bad" end
      if args.text == "odd" then return "unsure", "it is odd" end
      return "safe"
    end },
    execute = function(args, call)
      order[#order + 1] = "execute"
      seen.execute_note = call.note
      call.ok(args.text)
    end,
  })) } }

  assert(phased.receive({ kind = "phased.tool.prepare", id = "p1", name = "echo",
    args = { text = "fine" }, approval_policy = "ReadOnly" }), "prepare is routed")
  eq(table.concat(order, ","), "prepare,decide", "prepare runs before the decision, and execute does not run")
  eq(seen.policy, "ReadOnly", "the decision function receives the caller's policy")
  eq(seen.call_policy, "ReadOnly", "the call carries the caller's policy")
  eq(seen.prepare_phase, "prepare", "prepare runs in the prepare phase")
  eq(seen.decide_note, "kept:fine", "the decision function sees what prepare left on the call")
  eq(last_prepared("p1").decision, "safe", "the decision is answered as tool.prepared")
  eq(last_prepared("p1").reason, nil, "a safe decision needs no reason")
  eq(last_result("p1"), nil, "preparing settles nothing")

  phased.receive({ kind = "phased.tool.invoke", id = "p1", name = "echo",
    args = { text = "fine" }, approval_policy = "ReadOnly" })
  eq(table.concat(order, ","), "prepare,decide,execute", "execute runs only when invoked")
  eq(seen.execute_note, "kept:fine", "execute runs on the call prepare opened")
  eq(last_result("p1").output, "fine", "execute settles the call")

  phased.receive({ kind = "phased.tool.prepare", id = "p2", name = "echo",
    args = { text = "bad" }, approval_policy = "Default" })
  eq(last_prepared("p2").decision, "denied", "a denial is answered, not enforced, by the tool")
  eq(last_prepared("p2").reason, "it is bad", "with its reason")
  phased.receive({ kind = "phased.tool.prepare", id = "p3", name = "echo",
    args = { text = "odd" }, approval_policy = "Default" })
  eq(last_prepared("p3").decision, "unsure", "unsure is answered")
  eq(last_prepared("p3").reason, "it is odd", "with its reason")

  -- Invalid arguments and unusable tools end the call before prepare.
  local before = #order
  phased.receive({ kind = "phased.tool.prepare", id = "p4", name = "echo", args = { nope = 1 } })
  eq(last_result("p4").error, "echo: unknown argument `nope`; accepted arguments: text, times",
    "invalid arguments are rejected before prepare")
  phased.receive({ kind = "phased.tool.prepare", id = "p5", name = "echo", args = { text = "x" }, allowlist = {} })
  eq(last_result("p5").error, "unknown tool `echo`; no tools are available", "an excluded tool is not prepared")
  eq(#order, before, "neither reached a tool function")
  eq(last_prepared("p4"), nil, "a rejected call is not prepared")

  -- A cancelled prepared call is dropped; a later invoke starts a fresh call.
  phased.receive({ kind = "phased.tool.prepare", id = "p6", name = "echo", args = { text = "fine" } })
  assert(phased.receive({ kind = "phased.tool.cancel", id = "p6" }), "a prepared call can be cancelled")
  eq(last_result("p6"), nil, "cancelling a prepared call emits nothing")
end

-- A tool without a decision function answers without a decision, under every
-- policy; a tool whose function returns nothing does the same.
do
  local silent = tool.source { name = "silent", tools = {
    tool.define(spec()),
    tool.define(spec({ name = "partial", approval = { decide = tool.approval.reads() } })),
  } }
  for index, policy in ipairs({ "ReadOnly", "Default" }) do
    local id = "s" .. index
    silent.receive({ kind = "silent.tool.prepare", id = id, name = "echo",
      args = { text = "x" }, approval_policy = policy })
    assert(last_prepared(id) ~= nil, "a tool without a decision function still answers")
    eq(last_prepared(id).decision, nil, "and says nothing under " .. policy)
  end
  silent.receive({ kind = "silent.tool.prepare", id = "s3", name = "partial",
    args = { text = "x" }, approval_policy = "ReadOnly" })
  eq(last_prepared("s3").decision, "safe", "a reading tool is safe under ReadOnly")
  silent.receive({ kind = "silent.tool.prepare", id = "s4", name = "partial",
    args = { text = "x" }, approval_policy = "Default" })
  assert(last_prepared("s4") ~= nil, "it still answers under Default")
  eq(last_prepared("s4").decision, nil, "and leaves Default to the gate's configured setting")
  local writes = tool.approval.writes("it writes")
  local result, reason = writes("ReadOnly")
  eq(result, "denied", "a writing tool is denied under ReadOnly")
  eq(reason, "it writes", "with the declared reason")
  eq(writes("Default"), nil, "and says nothing under Default")
end

-- Deferred decisions, failures before execute, and misuse.
do
  local held
  local slow = tool.source { name = "slow", tools = {
    tool.define(spec({ approval = { decide = function(_, _, call) held = call; return tool.deferred end } })),
    tool.define(spec({ name = "refuse", prepare = function(_, call) call.err("refuse: no such file") end,
      approval = { decide = function() error("decide must not run after prepare failed") end } })),
    tool.define(spec({ name = "raise", approval = { decide = function() error("boom", 0) end } })),
    tool.define(spec({ name = "wrong", approval = { decide = function() return "approve" end } })),
    tool.define(spec({ name = "early", prepare = function(_, call) call.ok("done") end })),
  } }
  slow.receive({ kind = "slow.tool.prepare", id = "d1", name = "echo", args = { text = "x" } })
  eq(last_prepared("d1"), nil, "a deferred decision answers nothing yet")
  held.decided("unsure", "still thinking")
  eq(last_prepared("d1").decision, "unsure", "the deferred decision is answered later")
  held.decided("safe")
  eq(last_prepared("d1").decision, "unsure", "a call is decided once")

  slow.receive({ kind = "slow.tool.prepare", id = "d2", name = "refuse", args = { text = "x" } })
  eq(last_result("d2").error, "refuse: no such file", "prepare may end the call with an error")
  eq(last_prepared("d2"), nil, "and then nothing is decided")
  slow.receive({ kind = "slow.tool.prepare", id = "d3", name = "raise", args = { text = "x" } })
  eq(last_result("d3").error, "raise: approval.decide raised: boom", "a raising decision ends the call")
  slow.receive({ kind = "slow.tool.prepare", id = "d4", name = "wrong", args = { text = "x" } })
  eq(last_result("d4").error, "wrong: approval decision `approve` is not one of safe, unsure, denied",
    "an unknown result ends the call instead of being guessed")
  slow.receive({ kind = "slow.tool.prepare", id = "d5", name = "early", args = { text = "x" } })
  assert(last_result("d5").error:find("a call can only succeed from execute", 1, true),
    "a tool cannot succeed before execute: " .. tostring(last_result("d5").error))
end

-- An invoke that was never prepared (a workflow node's request) runs execute.
do
  local direct_prepared = 0
  local direct = tool.source { name = "direct", tools = { tool.define(spec({
    prepare = function() direct_prepared = direct_prepared + 1 end,
    approval = { decide = function() error("not consulted") end },
  })) } }
  direct.receive({ kind = "direct.tool.invoke", id = "n1", name = "echo", args = { text = "node" } })
  eq(last_result("n1").output, "node", "execute runs")
  eq(direct_prepared, 0, "without a prepare request nothing is prepared or decided")
end

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
