-- Deterministic bash command reasoner for graph-enforced command execution.
-- Uses tool-gate -> bash like run.lua; does not grant direct bash access.
local envelope = require("core.envelope")
local event = require("core.event")
local replay_window = require("core.history_replay")
local run = require("reasoners.run")

local emit_as = envelope.emit_as
local emit = envelope.emit
local next_id = envelope.next_id

local M = {}
local tool_to_firing = {}
local firing_meta = {}

local function handle(body)
  local firing_id = body.firing_id
  local args = type(body.args) == "table" and body.args or {}
  local command = args.command
  if type(command) ~= "string" or #command == 0 then
    return "bash_command reasoner: args.command must be a non-empty string"
  end
  local tool_id = next_id("tool")
  tool_to_firing[tool_id] = firing_id
  firing_meta[firing_id] = { command = command, cwd = args.cwd }
  emit("tool-gate", {
    kind = "tool-gate.tool.invoke",
    id = tool_id,
    name = "bash",
    args = { command = command, cwd = args.cwd },
  })
  return nil
end

local function on_tool_result(body)
  local tool_id = body.id
  if type(tool_id) ~= "string" then return end
  local firing_id = tool_to_firing[tool_id]
  if firing_id == nil then return end
  tool_to_firing[tool_id] = nil
  local meta = firing_meta[firing_id] or {}
  firing_meta[firing_id] = nil

  if type(body.error) == "string" and #body.error > 0 then
    emit_as("bash_command", nil, { kind = "tool.result", id = firing_id, error = body.error })
    return
  end
  if type(body.output) ~= "string" then
    emit_as("bash_command", nil, { kind = "tool.result", id = firing_id, error = "bash_command reasoner: bash returned non-string output" })
    return
  end
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
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end
  local evt = event.decode(entry)
  if evt == nil or evt.kind ~= "tool.result" then return end
  if replay_window.active() then return end
  on_tool_result(evt.body)
end

M.handle = handle
M.receive_msg = receive_msg
M._internals = {
  tool_to_firing = tool_to_firing,
  firing_meta = firing_meta,
  on_tool_result = on_tool_result,
  reset = function()
    for k, _ in pairs(tool_to_firing) do tool_to_firing[k] = nil end
    for k, _ in pairs(firing_meta) do firing_meta[k] = nil end
  end,
}
return M
