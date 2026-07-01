-- Deterministic fan-in reasoner. Preserves upstream identity and order.
local envelope = require("core.envelope")
local output_persist = require("reasoners.output_persistence")
local emit_as = envelope.emit_as

local M = {}

local function handle(body)
  local firing_id = body.firing_id
  local inputs = type(body.inputs) == "table" and body.inputs or {}
  local ids = {}
  for upstream_id, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" then ids[#ids + 1] = upstream_id end
  end
  table.sort(ids)

  local items = {}
  local parts = {}
  for _, uid in ipairs(ids) do
    local entry = inputs[uid]
    local item = { id = uid }
    if entry.output ~= nil then
      item.output = entry.output
      local out = entry.output
      local txt = (type(out) == "table" and out.text) or (type(out) == "string" and out) or ""
      parts[#parts + 1] = "## " .. tostring(uid) .. "\n" .. tostring(txt)
    elseif entry.error ~= nil then
      item.error = entry.error
      parts[#parts + 1] = "## " .. tostring(uid) .. "\n[error] " .. tostring(entry.error)
    elseif entry.skipped ~= nil then
      item.skipped = true
      parts[#parts + 1] = "## " .. tostring(uid) .. "\n[skipped]"
    end
    items[#items + 1] = item
  end

  local result = output_persist.persist(body, {
    items = items,
    text = table.concat(parts, "\n\n"),
  })
  emit_as("accumulate", nil, {
    kind = "tool.result",
    id = firing_id,
    result = result,
  })
  return "_already_replied"
end

M.handle = handle
return M
