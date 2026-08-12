local M = {}

local ephemeral_kinds = {
  agents_md = true,
  compaction = true,
  error = true,
  graph_result = true,
  plan = true,
  status = true,
  toast = true,
  popup = true,
  sidebar = true,
  mag_activity = true,
}

local function canonical_entry(entry)
  if type(entry) ~= "table" then return nil end
  if entry.role == "user" and type(entry.message_id) == "string" then
    return {
      kind = "message", role = "user", message_id = entry.message_id,
      turn_id = entry.turn_id, text = entry.text or "",
    }
  end
  if entry.role == "assistant" and type(entry.message_id) == "string" then
    return {
      kind = "message", role = "assistant", message_id = entry.message_id,
      turn_id = entry.turn_id, text = entry.text or "", reasoning = entry.reasoning and entry.reasoning.text or nil,
    }
  end
  if entry.kind == "tool_call" and type(entry.exchange_id or entry.id) == "string" then
    return {
      kind = "exchange", exchange_id = entry.exchange_id or entry.id,
      turn_id = entry.turn_id, name = entry.name, arguments = entry.raw_input,
      result = entry.output, error = entry.error == true,
    }
  end
  return nil
end

function M.normalize(state)
  local out = { canonical = {}, local_rows = {}, ephemeral = {}, unclassified = {} }
  for index, entry in ipairs((state or {}).entries or {}) do
    local canonical = canonical_entry(entry)
    if canonical then
      out.canonical[#out.canonical + 1] = canonical
    elseif type(entry) == "table" and entry.role == "user" and type(entry.local_id) == "string" then
      out.local_rows[#out.local_rows + 1] = {
        kind = entry.local_id == state.queued_entry_id and "queued_user" or "optimistic_user",
        local_id = entry.local_id, text = entry.text or "",
      }
    else
      local row = {
        index = index,
        kind = type(entry) == "table" and (entry.kind or entry.role or "unknown") or "unknown",
        explicitly_ephemeral = type(entry) == "table" and ephemeral_kinds[entry.kind] == true or false,
      }
      if row.explicitly_ephemeral then
        out.ephemeral[#out.ephemeral + 1] = row
      else
        out.unclassified[#out.unclassified + 1] = row
      end
    end
  end
  return out
end

return M
