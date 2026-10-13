-- Uniform tool constructor.
--
-- A tool is what a model sees and calls: a name, a description, a closed
-- parameter schema, a display contract, and two functions, `prepare` and
-- `execute`. Every tool in Nefor is built here; a tool backed by a process
-- capability is an ordinary tool whose `execute` sends a capability request.
--
--   local tool = require("libs.tool")
--   local approval = require("libs.tool.approval")
--   local write = tool.define {
--     name = "write_note",
--     description = "...",          -- string, or function(request) -> string
--     parameters = { type = "object", properties = { ... }, required = { ... } },
--     display = { compact = ..., expanded = ..., result = ... },
--     context = { folders = { ... } },  -- optional internal advertisement metadata
--     execution = { kind = "routed" },  -- optional gate execution metadata
--     approval = {                      -- optional; see "Approval" below
--       decide = function(policy, args, call)
--         if policy == approval.ReadOnly then return approval.denied, "it writes a note" end
--       end,
--       describe = { ReadOnly = "Your approval policy is ReadOnly, so this tool is rejected." },
--     },
--     prepare = nil,                                        -- or function(args, call)
--     execute = function(args, call) call.ok(output) end,
--     cancel = function(call) end,      -- optional
--   }
--
-- ## prepare and execute
--
-- Both functions belong to every tool's interface. `execute(args, call)` does
-- the tool's work and is required. `prepare(args, call)` is what the tool does
-- with a call before anyone has decided whether it may run, for example
-- storing the content the call carries so that it can be inspected; most
-- tools have none. Lua cannot tell a field set to nil from an absent field,
-- so "no prepare" is written either way, `prepare = nil` or nothing, and a
-- definition's `prepare` is then nil. What a caller does between the two
-- functions is the caller's concern. A source runs them in this order for one
-- call: `prepare` (if any), then the decision of whoever asked, then `execute`.
--
-- ## Approval
--
-- `approval.decide(policy, args, call)` says what one call means under the
-- caller's approval policy (`libs.tool.approval`): it returns `safe`, `unsure`
-- or `denied`, with a reason for the last two. Returning nothing means the
-- tool does not say; the gate then uses its configured setting for the tool
-- under `Default` and denies under any narrower policy. A decision that needs
-- to wait returns `tool.deferred` and later calls `call.decided(result,
-- reason)`. `approval.describe[policy]` is text appended to the tool's
-- description for an agent with that policy; a policy with nothing to add has
-- no entry. The two live together so that what is enforced and what the model
-- is told cannot drift apart. The tool only states meaning: what happens to a
-- `safe`, `unsure` or `denied` call is the gate's decision.
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
  if spec.prepare ~= nil and type(spec.prepare) ~= "function" then
    fail(name, "prepare must be a function or nil")
  end
  if type(spec.execute) ~= "function" then fail(name, "execute must be a function") end
  if spec.cancel ~= nil and type(spec.cancel) ~= "function" then
    fail(name, "cancel must be a function")
  end
  local decide, describe
  if spec.approval ~= nil then
    if type(spec.approval) ~= "table" then fail(name, "approval must be a table") end
    for key in pairs(spec.approval) do
      if key ~= "decide" and key ~= "describe" then
        fail(name, "approval has unknown field `" .. tostring(key) .. "`")
      end
    end
    decide = spec.approval.decide
    if decide ~= nil and type(decide) ~= "function" then
      fail(name, "approval.decide must be a function(policy, args, call)")
    end
    if spec.approval.describe ~= nil then
      if type(spec.approval.describe) ~= "table" then
        fail(name, "approval.describe must map an approval policy to text")
      end
      describe = {}
      for policy, text in pairs(spec.approval.describe) do
        if not approval.is_policy(policy) then
          fail(name, "approval.describe names unknown approval policy `" .. tostring(policy) .. "`")
        end
        if type(text) ~= "string" or text == "" then
          fail(name, "approval.describe." .. policy .. " must be a non-empty string")
        end
        describe[policy] = text
      end
      if next(describe) == nil then describe = nil end
    end
  end

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
    decide = decide,
    policy_descriptions = describe,
  }

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
    -- Answer the decision a deferred `approval.decide` promised.
    function call.decided(result, reason)
      if pending[id] ~= call or call.phase ~= "prepare" then return end
      if result ~= nil and not approval.is_result(result) then
        call.err(definition.name .. ": approval decision `" .. tostring(result)
          .. "` is not one of " .. table.concat(approval.RESULTS, ", "))
        return
      end
      call.phase = "prepared"
      emit({ kind = "tool.prepared", id = id, decision = result,
        reason = type(reason) == "string" and reason ~= "" and reason or nil })
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
    if definition.prepare ~= nil then
      if not run(definition, call, "prepare", definition.prepare, args, call) then return true end
      if pending[call.id] ~= call then return true end
    end
    if definition.decide == nil then
      call.decided(nil)
      return true
    end
    local ok, result, reason = run(definition, call, "approval.decide",
      definition.decide, call.policy, args, call)
    if not ok or pending[call.id] ~= call or call.phase ~= "prepare" then return true end
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
