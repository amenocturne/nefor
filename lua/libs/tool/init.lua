-- Uniform tool constructor.
--
-- A tool is what a model sees and calls: a name, a description, a closed
-- parameter schema, a display contract, and two functions, `prepare` and
-- `execute`. Every tool in Nefor is built here; a tool backed by a process
-- capability is an ordinary tool whose `execute` sends a capability request.
--
--   local tool = require("libs.tool")
--   local write = tool.define {
--     name = "write_note",
--     description = "...",          -- string, or function(request) -> string
--     parameters = { type = "object", properties = { ... }, required = { ... } },
--     display = { compact = ..., expanded = ..., result = ... },
--     context = { folders = { ... } },  -- optional internal advertisement metadata
--     execution = { kind = "routed" },  -- optional gate execution metadata
--     approval = {                      -- one entry per approval policy; see below
--       ReadOnly = {
--         decide = { "denied", "it writes a note" },
--         describe = "Your approval policy is ReadOnly, so every call to this tool is rejected.",
--       },
--       Default = {
--         decide = function(args, call)
--           if args.draft then return "safe" end
--           return "unsure", "it publishes the note"
--         end,
--         describe = tool.nothing,
--       },
--     },
--     prepare = tool.nothing,                               -- or function(args, call)
--     execute = function(args, call) call.ok(output) end,
--     cancel = function(call) end,      -- optional
--   }
--
-- Nothing about a tool is implied. A definition that leaves out `prepare`, an
-- approval policy, a `decide` or a `describe` fails when the tool is defined,
-- naming the tool and what is missing. Lua cannot tell a field set to nil from
-- an absent one, so "this tool has none" is written with one explicit value,
-- `tool.nothing`.
--
-- ## prepare and execute
--
-- Both belong to every tool's interface. `execute(args, call)` does the
-- tool's work. `prepare(args, call)` is what the tool does with a call before
-- anyone has decided whether it may run, for example storing the content the
-- call carries so that it can be inspected; a tool without such a step
-- declares `prepare = tool.nothing`. What a caller does between the two is the
-- caller's concern. A source runs them in this order for one call: `prepare`,
-- then the decision of whoever asked, then `execute`.
--
-- ## Approval
--
-- `approval` has exactly one entry for every approval policy in
-- `libs.tool.approval`'s order. An entry says what a call means for a caller
-- with that policy, and what the model is told about it:
--
--   * `decide` is the result for every call, `"safe"` or `{ "unsure", reason }`
--     or `{ "denied", reason }`, or a function `(args, call) -> result, reason`
--     for a tool whose calls differ. `unsure` and `denied` always carry a
--     reason. A function that returns no result, or an unknown one, or
--     raises, ends the call with an error: failing to decide is never a
--     decision. A decision that needs to wait returns `tool.deferred` and
--     later calls `call.decided(result, reason)`.
--   * `describe` is text appended to the tool's description for an agent
--     with that policy, or `tool.nothing`.
--
-- The two live together so that what is enforced and what the model is told
-- cannot drift apart. The tool only states meaning: what happens to a `safe`,
-- `unsure` or `denied` call is the gate's decision.
--
-- `define` checks the schema (only the keywords `libs.tool.schema` supports)
-- and the display contract when the tool is defined. A source groups tools
-- under one bus name, advertises them to a gate, validates every call's
-- arguments against the tool's schema before either function runs, and emits
-- results:
--
--   local source = tool.source { name = "read-only-tools", tools = { read } }
--   source.advertise("tool-gate")      -- <gate>.tools.advertise
--   source.prepare(body)               -- <name>.tool.prepare body -> tool.prepared
--   source.invoke(body)                -- <name>.tool.invoke body  -> tool.result
--   source.cancel(body)                -- <name>.tool.cancel body
--   source.result(id, { output = x })  -- deferred settlement by invocation id
--
-- `<name>.tool.prepare { id, name, args, approval_policy, ... }` runs the
-- tool's `prepare` and its decision function and answers `tool.prepared { id,
-- decision?, reason? }`; a later `<name>.tool.invoke` with the same id runs
-- `execute` on the same call. An invoke that was never prepared (a workflow
-- node's capability request) runs `execute` directly.
--
-- Invalid arguments never reach a tool function; the caller receives
-- `tool.result { error = "<tool>: <problem>" }` with the problem worded by
-- `libs.tool.schema`. `execute` settles through `call.ok(output, fields)` or
-- `call.err(message, fields)` (extra `fields` ride on the result envelope),
-- may report process output with `call.stream(stream, text)`, and may keep
-- `call.id` to settle later through `source.result`. `prepare` and `decide`
-- may end the call with `call.err`; they cannot call `call.ok`.
--
-- A call naming a tool its caller cannot use gets one answer everywhere,
-- worded by `unknown_tool_message`: ``unknown tool `x`; available tools: a, b``.
-- A source lists its own tools, narrowed to the invocation's `allowlist` (in
-- that list's order) when the caller sent one; a tool the allowlist excludes
-- is answered exactly like a tool that does not exist. The tool gate words the
-- same message over the whole registry.

local json_data = require("core.json_data")
local envelope = require("core.envelope")
local replay_window = require("core.replay_window")
local tool_display = require("libs.chat.tool_display")
local schema = require("libs.tool.schema")
local approval = require("libs.tool.approval")

local M = {}

M.schema = schema
M.approval = approval

-- The explicit "this tool has none" for `prepare` and for a policy's
-- `describe`.
M.nothing = setmetatable({}, { __tostring = function() return "tool.nothing" end })

-- Returned by a decision function that settles later through `call.decided`.
M.deferred = setmetatable({}, { __tostring = function() return "tool.deferred" end })

local DEFINITION_FIELDS = {
  name = true, description = true, parameters = true, display = true,
  context = true, execution = true, approval = true,
  prepare = true, execute = true, cancel = true,
}

local function mark_array(value)
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.mark_array) == "function" then
    nefor.json.mark_array(value)
  end
  return value
end

-- Lua cannot tell `{}` from `[]`; mark the schema lists so an empty
-- `required` still encodes as a JSON array.
local function mark_schema_lists(node)
  if type(node) ~= "table" then return end
  for _, key in ipairs({ "required", "enum", "oneOf" }) do
    if type(node[key]) == "table" then mark_array(node[key]) end
  end
  for _, child in pairs(node.properties or {}) do mark_schema_lists(child) end
  for _, child in ipairs(node.oneOf or {}) do mark_schema_lists(child) end
  if type(node.items) == "table" then mark_schema_lists(node.items) end
end

local function fail(name, message)
  error("tool.define: " .. (name and ("`" .. name .. "`: ") or "") .. message, 3)
end

function M.define(spec)
  if type(spec) ~= "table" then error("tool.define: spec must be a table", 2) end
  local name = spec.name
  if type(name) ~= "string" or name == "" then fail(nil, "name must be a non-empty string") end
  for key in pairs(spec) do
    if not DEFINITION_FIELDS[key] then fail(name, "unknown field `" .. tostring(key) .. "`") end
  end
  local description = spec.description
  if type(description) ~= "string" and type(description) ~= "function" then
    fail(name, "description must be a string or a function(request) returning one")
  end
  local parameters = json_data.copy(spec.parameters)
  local ok, err = schema.check(parameters)
  if not ok then fail(name, err) end
  mark_schema_lists(parameters)
  ok, err = tool_display.validate(spec.display)
  if not ok then fail(name, "invalid display: " .. tostring(err)) end
  if spec.context ~= nil and type(spec.context) ~= "table" then fail(name, "context must be a table") end
  if spec.execution ~= nil and type(spec.execution) ~= "table" then
    fail(name, "execution must be a table")
  end
  if spec.prepare == nil then
    fail(name, "prepare is not defined; give a function, or tool.nothing for a tool without a prepare step")
  end
  if spec.prepare ~= M.nothing and type(spec.prepare) ~= "function" then
    fail(name, "prepare must be a function or tool.nothing")
  end
  if type(spec.execute) ~= "function" then fail(name, "execute must be a function") end
  if spec.cancel ~= nil and type(spec.cancel) ~= "function" then
    fail(name, "cancel must be a function")
  end
  if type(spec.approval) ~= "table" then
    fail(name, "approval is not defined; it needs one entry for each approval policy ("
      .. table.concat(approval.POLICIES, ", ") .. ")")
  end
  for key in pairs(spec.approval) do
    if not approval.is_policy(key) then
      fail(name, "approval names unknown approval policy `" .. tostring(key) .. "`")
    end
  end
  local decisions, describe = {}, {}
  for _, policy in ipairs(approval.POLICIES) do
    local entry = spec.approval[policy]
    if type(entry) ~= "table" then
      fail(name, "approval does not define the " .. policy .. " policy")
    end
    for key in pairs(entry) do
      if key ~= "decide" and key ~= "describe" then
        fail(name, "approval." .. policy .. " has unknown field `" .. tostring(key) .. "`")
      end
    end
    local decide = entry.decide
    if decide == nil then
      fail(name, "approval." .. policy .. ".decide is not defined")
    elseif type(decide) == "function" then
      decisions[policy] = decide
    else
      local result, reason = decide, nil
      if type(decide) == "table" then result, reason = decide[1], decide[2] end
      local problem = approval.check_decision(result, reason)
      if problem then fail(name, "approval." .. policy .. ".decide: " .. problem) end
      decisions[policy] = function() return result, reason end
    end
    local text = entry.describe
    if text == nil then
      fail(name, "approval." .. policy
        .. ".describe is not defined; give text, or tool.nothing when there is nothing to add")
    elseif text ~= M.nothing then
      if type(text) ~= "string" or text == "" then
        fail(name, "approval." .. policy .. ".describe must be a non-empty string or tool.nothing")
      end
      describe[policy] = text
    end
  end
  if next(describe) == nil then describe = nil end

  local definition = {
    name = name,
    description = description,
    parameters = parameters,
    display = json_data.copy(spec.display),
    context = spec.context and json_data.copy(spec.context) or nil,
    execution = spec.execution and json_data.copy(spec.execution) or nil,
    prepare = spec.prepare,
    execute = spec.execute,
    cancel = spec.cancel,
    policy_descriptions = describe,
  }

  -- What one call means for a caller with `policy`: `safe`, `unsure` or
  -- `denied` and a reason, or `tool.deferred`. Raises when the policy is not
  -- one, or when the tool's function does not produce a valid decision.
  function definition.decide(policy, args, call)
    local decide = decisions[policy]
    if decide == nil then
      error("no approval policy `" .. tostring(policy) .. "`; the caller must state one of "
        .. table.concat(approval.POLICIES, ", "), 0)
    end
    local result, reason = decide(args, call)
    if result == M.deferred then return result end
    local problem = approval.check_decision(result, reason)
    if problem then error("its " .. policy .. " decision is invalid: " .. problem, 0) end
    return result, reason
  end

  -- Model-facing definition. `request` reaches a description function so a
  -- request-local tool can describe per-request data; advertised tools use a
  -- static string.
  function definition.spec(request)
    local text = description
    if type(text) == "function" then text = text(request) end
    if type(text) ~= "string" then
      error("tool `" .. name .. "`: description function must return a string", 2)
    end
    return {
      name = name,
      description = text,
      parameters = json_data.copy(parameters),
      display = json_data.copy(definition.display),
      context = definition.context and json_data.copy(definition.context) or nil,
      execution = definition.execution and json_data.copy(definition.execution) or nil,
      policy_descriptions = describe and json_data.copy(describe) or nil,
    }
  end

  -- The text this tool adds to its description for an agent with `policy`,
  -- or nil when it adds nothing.
  function definition.describe(policy)
    return describe and describe[policy] or nil
  end

  -- nil when `args` satisfy the schema, otherwise the caller-facing message.
  function definition.check(args)
    if args == nil then args = {} end
    local problem = schema.validate(parameters, args)
    if problem == nil then return nil end
    return name .. ": " .. problem
  end

  return definition
end

local function copy_fields(fields, into)
  for key, value in pairs(fields or {}) do
    if key ~= "kind" and key ~= "id" then into[key] = value end
  end
  return into
end

-- The one answer to a call naming a tool its caller cannot use. `available`
-- lists the names that caller can call, in the order its model was offered
-- them. The tool gate words the same message; keep the two identical.
function M.unknown_tool_message(name, available)
  local head = "unknown tool `" .. tostring(name) .. "`; "
  if #available == 0 then return head .. "no tools are available" end
  return head .. "available tools: " .. table.concat(available, ", ")
end

function M.source(opts)
  if type(opts) ~= "table" then error("tool.source: options must be a table", 2) end
  local name = opts.name
  if type(name) ~= "string" or name == "" then
    error("tool.source: name must be a non-empty string", 2)
  end
  if type(opts.tools) ~= "table" then error("tool.source: tools must be a list", 2) end
  local emit = opts.emit or function(body) envelope.emit_as(name, nil, body) end
  if type(emit) ~= "function" then error("tool.source: emit must be a function", 2) end

  local ordered, by_name = {}, {}
  for _, definition in ipairs(opts.tools) do
    if type(definition) ~= "table" or type(definition.check) ~= "function" then
      error("tool.source `" .. name .. "`: tools must be built with tool.define", 2)
    end
    if by_name[definition.name] then
      error("tool.source `" .. name .. "`: tool `" .. definition.name .. "` defined twice", 2)
    end
    if type(definition.description) ~= "string" then
      error("tool.source `" .. name .. "`: advertised tool `" .. definition.name
        .. "` needs a static description", 2)
    end
    by_name[definition.name] = definition
    ordered[#ordered + 1] = definition
  end

  local pending = {}
  local source = { name = name, tools = ordered }

  function source.get(tool_name) return by_name[tool_name] end

  function source.advertisement(gate)
    local tools = {}
    for index, definition in ipairs(ordered) do tools[index] = definition.spec() end
    return {
      kind = gate .. ".tools.advertise",
      source = name,
      tools = mark_array(tools),
    }
  end

  function source.advertise(gate)
    if type(gate) ~= "string" or gate == "" then
      error("tool.source `" .. name .. "`: gate must be a non-empty string", 2)
    end
    emit(source.advertisement(gate))
  end

  -- Emit the result for invocation `id`. Fields other than kind/id are copied
  -- onto the envelope: `output` or `error`, plus any result metadata.
  function source.result(id, fields)
    pending[id] = nil
    emit(copy_fields(fields, { kind = "tool.result", id = id }))
  end

  -- One call of one tool. `call.phase` is "prepare" until the tool has
  -- answered `tool.prepared`, "prepared" while its caller decides, and
  -- "execute" once `execute` runs. `call.policy` is the caller's approval
  -- policy when the call carries one.
  local function new_call(definition, body, args)
    local id = body.id
    local call = {
      id = id, name = definition.name, args = args, request = body,
      policy = body.approval_policy,
    }
    function call.ok(output, fields)
      if pending[id] ~= call then return end
      if call.phase ~= "execute" then
        error("tool `" .. definition.name .. "`: a call can only succeed from execute", 2)
      end
      local result = copy_fields(fields, {})
      result.output = output
      result.error = nil
      source.result(id, result)
    end
    function call.err(message, fields)
      if pending[id] ~= call then return end
      local result = copy_fields(fields, {})
      result.error = tostring(message)
      result.output = nil
      source.result(id, result)
    end
    function call.stream(stream, text, fields)
      emit(copy_fields(fields, { kind = "tool.stream", id = id, stream = stream, text = text }))
    end
    function call.settled() return pending[id] ~= call end
    -- Answer the decision a deferred `decide` promised.
    function call.decided(result, reason)
      if pending[id] ~= call or call.phase ~= "prepare" then return end
      local problem = approval.check_decision(result, reason)
      if problem then
        call.err(definition.name .. ": this call could not be decided: its "
          .. tostring(call.policy) .. " decision is invalid: " .. problem)
        return
      end
      call.phase = "prepared"
      emit({ kind = "tool.prepared", id = id, decision = result, reason = reason })
    end
    return call
  end

  -- This source's tools the invocation may call. `allowlist` is the
  -- invocation's own list of tool names; without one every tool is callable.
  local function available_tools(allowlist)
    local names, seen = {}, {}
    if type(allowlist) ~= "table" then
      for index, definition in ipairs(ordered) do names[index] = definition.name end
      return names, by_name
    end
    for _, tool_name in ipairs(allowlist) do
      if by_name[tool_name] and not seen[tool_name] then
        seen[tool_name] = by_name[tool_name]
        names[#names + 1] = tool_name
      end
    end
    return names, seen
  end

  -- Resolve the tool a body names and check its arguments. Answers the call
  -- itself and returns nil when the tool is unusable or the arguments invalid.
  local function admit(body, label)
    if type(body) ~= "table" then return nil, false end
    local id = body.id
    if type(id) ~= "string" or id == "" then
      nefor.log.warn("tool.source: " .. label .. " without a string id dropped", { source = name })
      return nil, false
    end
    local available, callable = available_tools(body.allowlist)
    local definition = callable[body.name]
    if definition == nil then
      source.result(id, { error = M.unknown_tool_message(body.name, available) })
      return nil, true
    end
    local args = body.args
    if args == nil then args = {} end
    local problem = definition.check(args)
    if problem then
      source.result(id, { error = problem })
      return nil, true
    end
    return definition, true, args
  end

  -- Run one tool function for `call`. A raised error ends the call.
  local function run(definition, call, label, fn, ...)
    local ok, first, second = pcall(fn, ...)
    if ok then return true, first, second end
    if opts.propagate_errors then error(first, 0) end
    if type(first) == "table" and first.kind == "tool_output_unavailable" then error(first, 0) end
    if pending[call.id] == call then
      call.err(definition.name .. ": " .. label .. " raised: " .. tostring(first))
    else
      nefor.log.error("tool.source: " .. label .. " raised after settling", {
        source = name, tool = definition.name, error = tostring(first),
      })
    end
    return false
  end

  -- Handle a `<name>.tool.prepare` body: run the tool's `prepare`, then its
  -- decision function under the caller's approval policy, and answer
  -- `tool.prepared { id, decision?, reason? }`. Nothing here executes the
  -- tool. Returns true when the body was addressed to this source.
  function source.prepare(body)
    local definition, addressed, args = admit(body, "prepare")
    if definition == nil then return addressed end
    local call = new_call(definition, body, args)
    call.phase = "prepare"
    pending[call.id] = call
    if definition.prepare ~= M.nothing then
      if not run(definition, call, "prepare", definition.prepare, args, call) then return true end
      if pending[call.id] ~= call then return true end
    end
    -- Failing to decide is never a decision: it ends the call with an error.
    local ok, result, reason = pcall(definition.decide, call.policy, args, call)
    if not ok then
      if opts.propagate_errors and type(result) == "table" then error(result, 0) end
      if pending[call.id] == call then
        call.err(definition.name .. ": this call could not be decided: " .. tostring(result))
      end
      return true
    end
    if pending[call.id] ~= call or call.phase ~= "prepare" then return true end
    if result ~= M.deferred then call.decided(result, reason) end
    return true
  end

  -- Handle a `<name>.tool.invoke` body: run the tool's `execute`, on the call
  -- a `tool.prepare` with the same id opened when there is one. Returns true
  -- when it was addressed to one of this source's tools or answered as an
  -- unknown tool.
  function source.invoke(body)
    local definition, addressed, args = admit(body, "invoke")
    if definition == nil then
      if addressed and type(body.id) == "string" then pending[body.id] = nil end
      return addressed
    end
    local call = pending[body.id]
    if call ~= nil and call.name == definition.name and call.phase == "prepared" then
      -- An approval may have replaced the arguments; execute sees those.
      call.args, call.request = args, body
    else
      call = new_call(definition, body, args)
      pending[call.id] = call
    end
    call.phase = "execute"
    run(definition, call, "execute", definition.execute, args, call)
    return true
  end

  -- Handle a `<name>.tool.cancel` body for an unsettled invocation. Returns
  -- the cancelled call, or nil when none is pending under that id.
  function source.cancel(body)
    local id = type(body) == "table" and body.id or nil
    local call = id and pending[id] or nil
    if call == nil then return nil end
    pending[id] = nil
    local definition = by_name[call.name]
    -- `cancel` stops running work; a call that never reached execute has none.
    if definition and definition.cancel and call.phase == "execute" then
      local ok, raised = pcall(definition.cancel, call)
      if not ok then
        nefor.log.error("tool.source: cancel raised", {
          source = name, tool = call.name, error = tostring(raised),
        })
      end
    end
    return call
  end

  -- Dispatch a decoded bus body addressed to this source.
  function source.receive(body)
    local kind = type(body) == "table" and body.kind or nil
    if kind == name .. ".tool.prepare" then return source.prepare(body) end
    if kind == name .. ".tool.invoke" then return source.invoke(body) end
    if kind == name .. ".tool.cancel" then return source.cancel(body) ~= nil end
    return false
  end

  return source
end

-- The tool definitions of one provider request, for an agent with approval
-- policy `policy`. `names` are the registered tools the agent may call,
-- `registered` maps a tool name to its registered spec (as the gate's
-- `tool.register` lists it, `policy_descriptions` included), and `tool_specs`
-- are the request's own definitions. For every named tool whose spec has
-- text for `policy`, the result gains a request definition equal to the
-- registered one with that text appended to its description; a provider
-- prefers a request definition over its catalog entry of the same name and
-- owner. A tool with nothing to add is left alone. A request for an agent
-- must name its policy: anything else raises. This is the only place
-- per-policy text is appended.
function M.request_tool_specs(names, registered, tool_specs, policy)
  if not approval.is_policy(policy) then
    error("tool.request_tool_specs: the request names no approval policy (got `"
      .. tostring(policy) .. "`)", 2)
  end
  local specs = {}
  local defined = {}
  for index, spec in ipairs(tool_specs or {}) do
    specs[index] = spec
    if type(spec) == "table" and type(spec.name) == "string" then defined[spec.name] = true end
  end
  for _, tool_name in ipairs(names or {}) do
    local spec = registered[tool_name]
    local text = type(spec) == "table" and type(spec.policy_descriptions) == "table"
      and spec.policy_descriptions[policy] or nil
    if type(text) == "string" and text ~= "" and not defined[tool_name] then
      defined[tool_name] = true
      specs[#specs + 1] = {
        name = tool_name,
        owner = spec.owner,
        description = tostring(spec.description or "") .. "\n\n" .. text,
        parameters = json_data.copy(spec.parameters),
        execution = spec.execution and json_data.copy(spec.execution) or nil,
      }
    end
  end
  return specs
end

-- What a call of a named tool means under an approval policy, read from the
-- tools of `sources`: `decide(tool_name, policy, args) -> result, reason`.
-- It raises for a tool none of the sources define and for a call the tool
-- cannot decide. The workflow runner decides a workflow's capability nodes
-- with it, so a node and a tool call share one definition of each capability.
function M.decision(sources)
  if type(sources) ~= "table" then error("tool.decision: sources must be a list", 2) end
  return function(tool_name, policy, args)
    for _, source in ipairs(sources) do
      local definition = source.get(tool_name)
      if definition ~= nil then return definition.decide(policy, args) end
    end
    error("no tool `" .. tostring(tool_name) .. "` defines what this capability means", 0)
  end
end

-- Pure-Lua actor for a source whose tool functions run in this VM. It advertises on
-- every live `<gate>.hello` and serves prepares, invocations and cancellations; replayed
-- traffic is ignored.
function M.actor(opts)
  if type(opts) ~= "table" then error("tool.actor: options must be a table", 2) end
  local source = opts.source
  if type(source) ~= "table" or type(source.receive) ~= "function" then
    error("tool.actor: source must be built with tool.source", 2)
  end
  local gate = opts.gate
  if type(gate) ~= "string" or gate == "" then
    error("tool.actor: gate must be a non-empty string", 2)
  end
  local json = nefor.json
  local function receive_msg(entry)
    if entry.origin == "step" and entry.target ~= nil then return end
    local payload = entry.payload
    if type(payload) ~= "string" or payload == "" then return end
    local ok, decoded = pcall(json.decode, payload)
    if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
    if replay_window.active() then return end
    local body = decoded.body
    if body.kind == gate .. ".hello" then
      source.advertise(gate)
      return
    end
    source.receive(body)
  end
  return {
    name = source.name,
    source = source,
    receive_msg = receive_msg,
    send_msg = function(_) end,
  }
end

return M
