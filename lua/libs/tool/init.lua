-- Uniform tool constructor.
--
-- A tool is what a model sees and calls: a name, a description, a closed
-- parameter schema, a display contract, and a handler. Every tool in Nefor is
-- built here; a tool backed by a process capability is an ordinary tool whose
-- handler sends a capability request.
--
--   local tool = require("libs.tool")
--   local read = tool.define {
--     name = "read_file",
--     description = "...",          -- string, or function(request) -> string
--     parameters = { type = "object", properties = { ... }, required = { ... } },
--     display = { compact = ..., expanded = ..., result = ... },
--     context = { folders = { ... } },  -- optional internal advertisement metadata
--     execution = { kind = "routed" },  -- optional gate execution metadata
--     handler = function(args, call) call.ok(output) end,
--     cancel = function(call) end,      -- optional
--   }
--
-- `define` checks the schema (only the keywords `libs.tool.schema` supports)
-- and the display contract when the tool is defined. A source groups tools
-- under one bus name, advertises them to a gate, validates every invocation
-- against the tool's schema before its handler runs, and emits results:
--
--   local source = tool.source { name = "read-only-tools", tools = { read } }
--   source.advertise("tool-gate")      -- <gate>.tools.advertise
--   source.invoke(body)                -- <name>.tool.invoke body
--   source.cancel(body)                -- <name>.tool.cancel body
--   source.result(id, { output = x })  -- deferred settlement by invocation id
--
-- Invalid arguments never reach a handler; the caller receives
-- `tool.result { error = "<tool>: <problem>" }` with the problem worded by
-- `libs.tool.schema`. A handler settles through `call.ok(output, fields)` or
-- `call.err(message, fields)` (extra `fields` ride on the result envelope),
-- may report process output with `call.stream(stream, text)`, and may keep
-- `call.id` to settle later through `source.result`.

local json_data = require("core.json_data")
local envelope = require("core.envelope")
local replay_window = require("core.replay_window")
local tool_display = require("libs.chat.tool_display")
local schema = require("libs.tool.schema")

local M = {}

M.schema = schema

local DEFINITION_FIELDS = {
  name = true, description = true, parameters = true, display = true,
  context = true, execution = true, handler = true, cancel = true,
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
  if type(spec.handler) ~= "function" then fail(name, "handler must be a function") end
  if spec.cancel ~= nil and type(spec.cancel) ~= "function" then
    fail(name, "cancel must be a function")
  end

  local definition = {
    name = name,
    description = description,
    parameters = parameters,
    display = json_data.copy(spec.display),
    context = spec.context and json_data.copy(spec.context) or nil,
    execution = spec.execution and json_data.copy(spec.execution) or nil,
    handler = spec.handler,
    cancel = spec.cancel,
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
    }
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

  local function new_call(definition, body, args)
    local id = body.id
    local call = { id = id, name = definition.name, args = args, request = body }
    function call.ok(output, fields)
      if pending[id] ~= call then return end
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
    return call
  end

  -- Handle a `<name>.tool.invoke` body. Returns true when it was addressed to
  -- one of this source's tools or answered as an unknown tool.
  function source.invoke(body)
    if type(body) ~= "table" then return false end
    local id = body.id
    if type(id) ~= "string" or id == "" then
      nefor.log.warn("tool.source: invoke without a string id dropped", { source = name })
      return false
    end
    local definition = by_name[body.name]
    if definition == nil then
      source.result(id, { error = name .. ": unknown tool `" .. tostring(body.name) .. "`" })
      return true
    end
    local args = body.args
    if args == nil then args = {} end
    local problem = definition.check(args)
    if problem then
      source.result(id, { error = problem })
      return true
    end
    local call = new_call(definition, body, args)
    pending[id] = call
    local ok, raised = pcall(definition.handler, args, call)
    if not ok then
      if opts.propagate_handler_errors then error(raised, 0) end
      if type(raised) == "table" and raised.kind == "tool_output_unavailable" then error(raised, 0) end
      if pending[id] == call then
        call.err(definition.name .. ": handler raised: " .. tostring(raised))
      else
        nefor.log.error("tool.source: handler raised after settling", {
          source = name, tool = definition.name, error = tostring(raised),
        })
      end
    end
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
    if definition and definition.cancel then
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
    if kind == name .. ".tool.invoke" then return source.invoke(body) end
    if kind == name .. ".tool.cancel" then return source.cancel(body) ~= nil end
    return false
  end

  return source
end

-- Pure-Lua actor for a source whose handlers run in this VM. It advertises on
-- every live `<gate>.hello` and serves invocations and cancellations; replayed
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
