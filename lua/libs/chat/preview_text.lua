-- Preview-only byte budgets, not display columns or grapheme layout.
local M = {}
function M.clip(text, n)
  if text == nil then return "" end
  text = tostring(text):gsub("\n", " "):gsub("\r", " ")
  if #text <= n then return text end
  local cut = math.max(0, n - 1)
  -- If the next byte continues a scalar, floor the prefix to its start.
  while cut > 0 do
    local next_byte = text:byte(cut + 1)
    if next_byte < 128 or next_byte >= 192 then break end
    cut = cut - 1
  end
  return text:sub(1, cut) .. "…"
end
return M
