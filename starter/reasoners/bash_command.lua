-- Deterministic bash command reasoner for graph-enforced command execution.
-- Uses the same tool-gate -> bash path as run.lua; does not grant direct bash.
local run = require("reasoners.run")
local envelope = require("core.envelope")
local emit_as = envelope.emit_as

local M = {}
local firing_meta = {}

local function handle(body)
  local args = type(body.args) == "table" and body.args or {}
  local command = args.command
  if type(command) ~= "string" or #command == 0 then
    return "bash_command reasoner: args.command must be a non-empty string"
  end
  firing_meta[body.firing_id] = { command = command, cwd = args.cwd }
  return run.handle(body)
end

local function on_tool_result(body)
  local tool_id = body.id
  if type(tool_id) ~= "string" then return end
  local firing_id = run._internals.tool_to_firing[tool_id]
  if firing_id == nil then return end
  run._internals.tool_to_firing[tool_id] = nil

  local meta = firing_meta[firing_id]
  if meta == nil then return end
  firing_meta[firing_id] = nil

  if type(body.output) == "string" then
    local parsed = run._internals.parse_bash_output(body.output)
    emit_as("bash_command", nil, {
      kind = "tool.result",
      id = firing_id,
      result = {
        command = meta.command,
        cwd = meta.cwd,
        stdout = parsed.stdout,
        stderr = parsed.stderr,
        exit_code = parsed.exit_code,
      },
    })
  elseif type(body.error) == "string" and #body.error > 0 then
    emit_as("bash_command", nil, { kind = "tool.result", id = firing_id, error = body.error })
  else
    emit_as("bash_command", nil, {
      kind = "tool.result",
      id = firing_id,
      error = "bash_command reasoner: bash returned non-string output",
    })
  end
end

M.handle = handle
M.receive_msg = function(entry)
  local event = require("core.event")
  local evt = event.decode(entry)
  if evt and evt.kind == "tool.result" then on_tool_result(evt.body) end
end
M._internals = { firing_meta = firing_meta, on_tool_result = on_tool_result }
return M
