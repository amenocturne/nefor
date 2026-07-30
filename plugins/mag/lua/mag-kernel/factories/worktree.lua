local kinds = require("kinds")
local preview_components = require("preview-components")

local M = {}

local READY = "nefor.worktree.Ready"
local FAILED = "nefor.worktree.Failed"

local WORKTREE_TYPE = { kind="named", name="nefor.worktree.Worktree", arguments={} }
local ERROR_TYPE = { kind="named", name="nefor.worktree.WorktreeError", arguments={} }

local function declaration(config)
  local params = {
    repository = "string",
    path = "string",
    branch = "string",
  }
  if config.operation == "create" then
    params.base = "string"
  end
  return {
    name = config.factory,
    preview = preview_components.input_output(),
    semantic = {
      input={kind="primitive",name="Unit"},
      output=WORKTREE_TYPE,
      inputs={{wire="mag.Unit",type={kind="primitive",name="Unit"}}},
      outputs={
        {wire=READY,type=WORKTREE_TYPE},
        {wire=FAILED,type=ERROR_TYPE},
      },
    },
    params = params,
    inputs = { input = "mag.Unit" },
    outputs = { READY, FAILED },
    signals = { "kill" },
  }
end

local function validate_params(config, params)
  for _, key in ipairs({ "repository", "path", "branch" }) do
    if type(params[key]) ~= "string" or params[key] == "" then
      return nil, string.format("%s actor requires non-empty params.%s", config.factory, key)
    end
  end
  if config.operation == "create"
      and (type(params.base) ~= "string" or params.base == "") then
    return nil, string.format("%s actor requires non-empty params.base", config.factory)
  end
  return true
end

function M.build(config)
  local factory = { declaration = declaration(config) }

  function factory.construct(id, params, emit)
    params = params or {}
    local ok, err = validate_params(config, params)
    if not ok then return nil, err end

    local pending = false
    local instance = { id = id }

    local function sign(message)
      message.from = id
      return message
    end

    local function fail(kind, message)
      return {
        status = "failed",
        failure = FAILED,
        value = {
          operation = config.operation,
          kind = kind,
          message = message,
          error = message,
        },
      }
    end

    local function handle_reply(activation)
      if not pending then return nil end
      pending = false
      if activation.error ~= nil then
        return fail("git-failed", tostring(activation.error))
      end
      local result = activation.result
      if type(result) ~= "table" then
        return fail("git-failed", "git-worktree capability returned a non-object result")
      end
      if result.ok ~= true then
        local failure = type(result.error) == "table" and result.error or {}
        return fail(
          type(failure.kind) == "string" and failure.kind or "git-failed",
          type(failure.message) == "string" and failure.message or "git-worktree operation failed"
        )
      end
      if type(result.worktree) ~= "table" then
        return fail("git-failed", "git-worktree capability omitted worktree result")
      end
      emit(sign({ kind = READY, value = result.worktree }))
      return { status = "ok" }
    end

    function instance.deliver(activation)
      activation = activation or {}
      if activation.kind == "reply" then
        return handle_reply(activation)
      end
      pending = true
      emit(sign({
        kind = "capability.invoke",
        capability = config.tool,
        request = { name = config.tool, args = params },
        ref = { operation = config.operation },
      }))
      return { status = "pending" }
    end

    function instance.handle_kill()
      pending = false
    end

    emit(sign({ kind = kinds.ready }))
    return instance
  end

  return factory
end

return M
