-- Adjacent bus consumers often parse the same payload. Keep only the most
-- recent parse, never a history-sized cache. Each caller still owns its tables;
-- only immutable strings are shared, including large tool output strings.
local json = nefor.json
local json_data = require("core.json_data")
local previous_payload, previous_value

local M = {}

function M.owned(payload)
  if payload ~= previous_payload or previous_payload == nil then
    local value = json.decode(payload)
    previous_payload, previous_value = payload, value
  end
  return json_data.copy(previous_value)
end

return M
