local display = require("libs.conversation-manager.display")

local M = {}

-- A prompt is authored only when its producer explicitly records the text.
-- Structured values without this evidence remain ordinary structured values.
function M.text(message)
  if type(message) ~= "table" or message.role ~= "user"
      or type(message.authored_prompt) ~= "string" then return nil end
  return message.authored_prompt
end

function M.provider_content(message, canonical_content)
  return M.text(message) or canonical_content
end

function M.display_text(message)
  if type(message) ~= "table" then return "" end
  local prompt = M.text(message)
  if prompt ~= nil then return prompt end
  if type(message.text) == "string" and message.text ~= "" then return message.text end
  if type(message.structured) == "table" and #message.structured == 1 then
    return display.structured_text(message.structured[1]) or ""
  end
  return ""
end

return M
