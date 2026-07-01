-- Deterministic retry/pass-through router. Intended for connected graph loops.
local envelope = require("core.envelope")
local output_persist = require("libs.output-persistence")
local emit_as = envelope.emit_as

local M = {}
local HARD_CAP = 6 -- less than 7

local function first_input(inputs)
  local ids = {}
  for id, entry in pairs(inputs or {}) do
    if type(entry) == "table" then ids[#ids + 1] = id end
  end
  table.sort(ids)
  if #ids == 0 then return nil end
  return ids[1], inputs[ids[1]]
end

local function handle(body)
  local firing_id = body.firing_id
  local args = type(body.args) == "table" and body.args or {}
  local prev_state = type(body.prev_state) == "table" and body.prev_state or {}
  local max_attempts = tonumber(args.max_attempts) or 3
  if max_attempts > HARD_CAP then max_attempts = HARD_CAP end
  if max_attempts < 1 then max_attempts = 1 end

  local inputs = type(body.inputs) == "table" and body.inputs or {}
  local upstream_id, entry = first_input(inputs)
  local attempt = (tonumber(prev_state.attempt) or 0) + 1
  local ok = type(entry) == "table" and entry.output ~= nil and entry.output.ok == true
  local exhausted = attempt >= max_attempts
  local route = ok and "pass" or (exhausted and "exhausted" or "retry")

  local output = {
    route = route,
    attempt = attempt,
    max_attempts = max_attempts,
    exhausted = (not ok) and exhausted,
    upstream_id = upstream_id,
    input = entry,
    traits = {
      pass_through = true,
      branch_routing = true,
    },
  }
  if route == "pass" and type(entry) == "table" then output.passthrough = entry.output end
  output = output_persist.persist(body, output)

  emit_as("retry", nil, {
    kind = "tool.result",
    id = firing_id,
    result = output,
  })
  return "_already_replied"
end

M.handle = handle
M.HARD_CAP = HARD_CAP
return M
