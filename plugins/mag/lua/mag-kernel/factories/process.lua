local kinds = require("kinds")

local M = {}

local RESULT_WIRE = "nefor.process.Result"
local FAILURE_WIRE = "nefor.process.CapabilityFailed"
local INPUTS = { "nefor.process.Input" }

local RESULT_TYPE = { kind="named", name="nefor.contracts.ProcessResult", arguments={} }
local FAILURE_TYPE = { kind="named", name="nefor.contracts.ProcessFailure", arguments={} }
local INPUT_TYPE = {kind="union",items={
  {kind="primitive",name="Unit"},
  {kind="named",name="nefor.contracts.Text",arguments={}},
}}

local function timeout_ms(timeout, actor)
  if type(timeout) ~= "table" or type(timeout.present) ~= "boolean"
      or type(timeout.milliseconds) ~= "number" then
    return nil, actor .. " params.timeout must be a Timeout record"
  end
  if timeout.present then
    if timeout.milliseconds < 1 or timeout.milliseconds % 1 ~= 0 then
      return nil, actor .. " params.timeout must contain a positive integer number of milliseconds"
    end
    return timeout.milliseconds
  end
  if timeout.milliseconds ~= 0 then
    return nil, actor .. " params.timeout must use milliseconds 0 when absent"
  end
  return nil
end

local function validate_common(params, actor)
  if type(params.cwd) ~= "string" or params.cwd == "" then
    return nil, actor .. " requires a non-empty string params.cwd"
  end
  local _, err = timeout_ms(params.timeout, actor)
  if err then return nil, err end
  return params.timeout
end

local function validate_exec(params)
  local timeout, err = validate_common(params, "process-exec actor")
  if err then return nil, err end
  if type(params.argv) ~= "table" or #params.argv == 0 then
    return nil, "process-exec actor requires a non-empty list params.argv"
  end
  for index, argument in ipairs(params.argv) do
    if type(argument) ~= "string" then
      return nil, string.format("process-exec actor params.argv[%d] must be a string", index)
    end
  end
  return { argv = params.argv, cwd = params.cwd, timeout = timeout }
end

local function validate_script(params)
  local timeout, err = validate_common(params, "shell-script actor")
  if err then return nil, err end
  if type(params.script) ~= "string" or params.script == "" then
    return nil, "shell-script actor requires a non-empty string params.script"
  end
  return { script = params.script, cwd = params.cwd, timeout = timeout }
end

local function declaration(name, identity, params_type)
  return {
    name = name,
    semantic = {
      input = INPUT_TYPE,
      output = RESULT_TYPE,
      inputs = {
        {wire="nefor.process.Input",type=INPUT_TYPE},
      },
      outputs = {
        {wire=RESULT_WIRE,type=RESULT_TYPE},
        {wire=FAILURE_WIRE,type=FAILURE_TYPE},
      },
    },
    params = params_type,
    inputs = { input = INPUTS },
    outputs = { RESULT_WIRE, FAILURE_WIRE },
    signals = { "kill" },
    identity = identity,
  }
end

local function factory(config)
  local factory_module = {
    declaration = declaration(config.name, config.identity, config.params),
  }

  function factory_module.construct(id, params, emit, deps)
    params = params or {}
    local args, validation_error = config.validate(params)
    if validation_error then return nil, validation_error end

    local function sign(message)
      message.from = id
      return message
    end

    local pending = {}
    local seq = 0
    local instance = { id = id }

    local function handle_reply(activation)
      local ref = activation.ref or {}
      if not pending[ref.call] then return nil end
      pending[ref.call] = nil

      if activation.error ~= nil then
        return {
          status = "failed",
          failure = FAILURE_WIRE,
          value = { operation = config.capability, error = tostring(activation.error) },
        }
      end
      local result = activation.result
      local termination = type(result) == "table" and result.termination or nil
      local termination_kind = type(termination) == "table" and termination.kind or nil
      local termination_value
      if termination_kind == "code" then
        termination_value = termination.code
      elseif termination_kind == "signal" then
        termination_value = termination.signal
      end
      if type(result) ~= "table"
          or type(result.stdout) ~= "string"
          or type(result.stderr) ~= "string"
          or (termination_kind ~= "code" and termination_kind ~= "signal")
          or type(termination_value) ~= "number"
          or termination_value % 1 ~= 0 then
        return {
          status = "failed",
          failure = FAILURE_WIRE,
          value = {
            operation = config.capability,
            error = "capability returned malformed ProcessResult",
          },
        }
      end

      if deps and type(deps.diagnostic) == "function" then
        deps.diagnostic({ kind = "process_exit", [termination_kind] = termination_value })
      end
      emit(sign({ kind = RESULT_WIRE, value = {
        stdout = result.stdout, stderr = result.stderr,
        termination = { kind = termination_kind, value = termination_value },
      }}))
      return { status = "ok" }
    end

    local function handle_input(one)
      local request_args = {}
      for key, value in pairs(args) do request_args[key] = value end
      local arrival = one.arrival or {}
      local constructor = arrival.constructor_id or arrival.type_id
      local descriptor = arrival.type or {}
      if constructor == "nefor.contracts.Text" or descriptor.name == "nefor.contracts.Text" then
        local message = one.message or {}
        local value = message.value
        request_args.stdin = type(value) == "table" and value.content or message.content or ""
      end
      seq = seq + 1
      pending[seq] = true
      emit(sign({
        kind = "capability.invoke",
        capability = config.capability,
        request = { name = config.capability, args = request_args },
        ref = { call = seq },
      }))
      return { status = "pending" }
    end

    function instance.deliver(activation)
      activation = activation or {}
      if activation.kind == "reply" then return handle_reply(activation) end
      return handle_input((activation.messages or {})[1] or {})
    end

    function instance.handle_kill()
      pending = {}
    end

    emit(sign({ kind = kinds.ready }))
    return instance
  end

  return factory_module
end

M.exec = factory({
  name = "process-exec",
  identity = "nefor.factory.process-exec",
  params = { argv = "string[]", cwd = "string", timeout = "record" },
  capability = "process.exec",
  validate = validate_exec,
})

M.script = factory({
  name = "shell-script",
  identity = "nefor.factory.shell-script",
  params = { script = "string", cwd = "string", timeout = "record" },
  capability = "shell.script",
  validate = validate_script,
})

return M
