-- Bus event decoding helpers.
--
-- Actors receive raw session-log entries. This module is the boundary
-- where that untrusted shape becomes a trusted event body.

local M = {}

local json = nefor.json

function M.decode(entry)
  if type(entry) ~= "table" then
    return nil, "entry is not a table"
  end

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then
    return nil, "entry.payload is not a non-empty string"
  end

  local ok, decoded = pcall(json.decode, payload)
  if not ok then
    return nil, "entry.payload is not valid JSON: " .. tostring(decoded)
  end
  if type(decoded) ~= "table" then
    return nil, "decoded payload is not a table"
  end

  local body = decoded.body
  if type(body) ~= "table" then
    return nil, "decoded.body is not a table"
  end

  local kind = body.kind
  if type(kind) ~= "string" or kind == "" then
    return nil, "decoded.body.kind is not a non-empty string"
  end

  return {
    decoded = decoded,
    body    = body,
    kind    = kind,
  }, nil
end

return M
