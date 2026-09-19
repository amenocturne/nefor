local display = require("libs.conversation-manager.display")

local M = {}

-- This is the single interpretation boundary for human-authored prompts.
-- Explicit presentation metadata wins over ordinary text chunks; structured
-- values alone never become prompts merely because they contain likely fields.
function M.text(message)
  if type(message) ~= "table" or message.role ~= "user" then return nil end
  if type(message.authored_prompt) == "string" then return message.authored_prompt end
  if type(message.text) == "string" and message.text ~= "" then return message.text end
  local chunks = {}
  for _, chunk in ipairs(message.chunks or {}) do
    if chunk.kind == "text" and type(chunk.data) == "string" then
      chunks[#chunks + 1] = chunk.data
    end
  end
  if #chunks > 0 then return table.concat(chunks) end
  return nil
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
