-- starter/reasoners/run.lua — generic `run(bash)` primitive reasoner.
--
-- A thin pass-through that wraps the existing `bash` tool (advertised
-- by basic-tools) with a typed-result envelope so graph topologies can
-- branch on `exit_code`. The orchestrator never invokes `run` with
-- arbitrary commands; concrete project-specific wrappers (see
-- `run-wrappers.lua`) sit on top and resolve a typed name to a
-- command string.
--
-- ## Dispatch envelope
--
--   tool.invoke {
--     id   = <firing_id>,
--     name = "run",
--     args = {
--       run_id   = <string>,
--       node_id  = <string>,
--       args     = { command = <string> },
--       inputs   = { ... },         -- ignored
--       prev_state = ...,           -- ignored (single-firing reasoner)
--     }
--   }
--
-- ## Reply envelope (terminal)
--
--   tool.result {
--     id     = <firing_id>,
--     result = {
--       stdout    = <string>,
--       stderr    = <string>,
--       exit_code = <int>,
--     }
--   }
--
-- ## Internal flow
--
-- On dispatch:
--   1. mint a fresh tool_id, store firing_id under tool_to_firing[tool_id]
--   2. emit `tool-gate.tool.invoke { id=tool_id, name="bash",
--                                    args={ command } }`
--   3. return — reasoners/init.lua hands control back to the bus
--
-- On `tool.result { id=tool_id }`:
--   - parse the bash plugin's combined output (see `parse_bash_output`)
--     back into stdout/stderr/exit_code fields
--   - emit `tool.result { id=firing_id, result={...} }` as the `run`
--     reasoner type so reasoner-graph closes the firing
--
-- The bash tool's output format (defined in plugins/basic-tools/src/
-- tools/bash.rs::format_output) is:
--
--   <stdout chunk, possibly empty>
--   [stderr]
--   <stderr chunk, possibly empty>
--   [exit N]
--
-- The split is lossy but unambiguous: the literal `[stderr]` line and
-- the trailing `[exit N]` footer are produced verbatim by the plugin.
-- A failed-spawn `tool.result` carries `error` instead of `output`; we
-- forward those as a structured `error` field.

local envelope      = require("core.envelope")
local event         = require("core.event")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit
local next_id = envelope.next_id

local M = {}

-- tool_to_firing[tool_id] = firing_id
local tool_to_firing = {}

-- Bash output parser.
--
-- Split the bash plugin's combined output into { stdout, stderr,
-- exit_code }. Exit footer is always present; "[stderr]" marker is
-- only present when stderr was non-empty.
local function parse_bash_output(text)
  local stdout = ""
  local stderr = ""
  local exit_code = nil

  -- Pull off the "[exit N]" footer (always last line, no trailing
  -- newline per format_output).
  local body, code_str = string.match(text, "^(.-)%[exit ([^%]]+)%]$")
  if body == nil then
    -- Footer absent / malformed; surface raw text as stdout.
    return { stdout = text, stderr = "", exit_code = nil }
  end
  exit_code = tonumber(code_str)

  -- Split on the literal "[stderr]\n" marker, if present. format_output
  -- only writes the marker when stderr was non-empty, so absence means
  -- stderr is empty. body may end with a newline before the marker.
  local stdout_part, stderr_part = string.match(body, "^(.-)%[stderr%]\n(.*)$")
  if stdout_part ~= nil then
    stdout = stdout_part
    stderr = stderr_part
  else
    stdout = body
  end

  -- format_output strips no trailing newlines; preserve exact bytes.
  return { stdout = stdout, stderr = stderr, exit_code = exit_code }
end

-- Dispatch handler — called from reasoners/init.lua.

-- Returns nil on accept (reply lands later via the bus), or a string
-- error to synth-fail the firing.
local function handle(body)
  local firing_id = body.firing_id
  local args = body.args
  if type(args) ~= "table" then
    return "run reasoner: missing args"
  end
  local command = args.command
  if type(command) ~= "string" or #command == 0 then
    return "run reasoner: args.command must be a non-empty string"
  end

  local tool_id = next_id("tool")
  tool_to_firing[tool_id] = firing_id

  emit("tool-gate", {
    kind = "tool-gate.tool.invoke",
    id   = tool_id,
    name = "bash",
    args = { command = command },
  })

  return nil
end

M.handle = handle

-- Bus event handler — tool.result correlation by tool_id.

local function on_tool_result(body)
  local tool_id = body.id
  if type(tool_id) ~= "string" then return end
  local firing_id = tool_to_firing[tool_id]
  if firing_id == nil then return end

  -- Always drop the mapping — one tool_id, one result.
  tool_to_firing[tool_id] = nil

  -- Surface infrastructure errors (e.g. tool-gate denied the call,
  -- spawn failed, tool unknown) verbatim. parse_bash_output is only
  -- meaningful for successful invocations.
  if type(body.error) == "string" and #body.error > 0 then
    emit_as("run", nil, {
      kind  = "tool.result",
      id    = firing_id,
      error = body.error,
    })
    return
  end

  local output = body.output
  if type(output) ~= "string" then
    emit_as("run", nil, {
      kind  = "tool.result",
      id    = firing_id,
      error = "run reasoner: bash returned non-string output",
    })
    return
  end

  local parsed = parse_bash_output(output)
  emit_as("run", nil, {
    kind   = "tool.result",
    id     = firing_id,
    result = {
      stdout    = parsed.stdout,
      stderr    = parsed.stderr,
      exit_code = parsed.exit_code,
    },
  })
end

local function receive_msg(entry)
  -- Matches the broadcast-fan-out filter in reasoners/init.lua.
  if entry.origin == "step" and entry.target ~= nil then return end

  local evt = event.decode(entry)
  if evt == nil then return end

  -- Replayed envelopes from a past run-id can't advance the in-memory
  -- tool_to_firing map; skip during replay (parity with agent.lua).
  if replay_window.active() then return end

  local body = evt.body
  if evt.kind ~= "tool.result" then return end
  on_tool_result(body)
end

M.receive_msg = receive_msg

M._internals = {
  tool_to_firing  = tool_to_firing,
  parse_bash_output = parse_bash_output,
  on_tool_result  = on_tool_result,
  reset = function()
    for k, _ in pairs(tool_to_firing) do tool_to_firing[k] = nil end
  end,
}

return M
