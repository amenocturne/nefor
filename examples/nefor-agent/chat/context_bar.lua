-- Starter opinion: token regions are backgrounds, not block-glyph progress.
local common = require("libs.chat.common")
local M = {}

local COLORS = {
  blue = { filled = "#5F87D7", empty = "#20304D", label = "#AFCAFF" },
  yellow = { filled = "#D7AF5F", empty = "#463920", label = "#FFE0A0" },
  red = { filled = "#D75F5F", empty = "#462424", label = "#FFB0B0" },
}

local function integer(n, minimum)
  return type(n) == "number" and n == n and n < math.huge
    and n >= minimum and n == math.floor(n)
end

local function count(n)
  return common.humanize_tokens(n)
end

local function regions(regular, maximum)
  if not integer(regular, 1) then regular = nil end
  if not integer(maximum, 1) or (regular and maximum < regular) then maximum = nil end
  local capacity = maximum or regular
  if not capacity then return {}, nil end
  if not regular or capacity == regular then
    return { { limit = capacity, color = COLORS.blue } }, capacity
  end
  -- For small windows, 10k rounding can erase the warning/red region.
  -- Fall back to token rounding there; collapse boundaries at the default.
  local red = math.floor(capacity * 0.85 / 10000 + 0.5) * 10000
  if red <= 0 or red >= capacity then red = math.floor(capacity * 0.85 + 0.5) end
  red = math.max(regular, math.min(capacity, red))
  local out = { { limit = regular, color = COLORS.blue } }
  if red > regular and red < capacity then
    out[#out + 1] = { limit = red, color = COLORS.yellow }
  end
  out[#out + 1] = { limit = capacity, color = COLORS.red }
  return out, capacity
end

local function widths(regions, capacity, requested)
  local minimum, prior, duplicate = 0, 0, {}
  for _, region in ipairs(regions) do
    region.label = count(region.limit)
    duplicate[region.label] = (duplicate[region.label] or 0) + 1
  end
  for _, region in ipairs(regions) do
    -- Nearby capacities must not look like duplicate thresholds after humanizing.
    if duplicate[region.label] > 1 then region.label = tostring(region.limit) end
    minimum = minimum + #region.label
  end
  -- 28 colored cells fits the 272k/740k/872k example (9/15/4).
  -- One window uses 30 (the same width including dividers). Narrow requests
  -- stop at label-fit minimum; labels never cross a divider. Dividers add n-1 cells.
  local target = math.max(minimum, math.floor(requested or (30 - (#regions - 1))))
  local allocated = 0
  for _, region in ipairs(regions) do
    region.width = math.max(#region.label,
      math.floor(target * (region.limit - prior) / capacity + 0.5))
    prior = region.limit
    allocated = allocated + region.width
  end
  -- Correct rounding/minimum-label adjustments using the region with most
  -- room. Geometry depends only on capacities, never on the moving usage.
  while allocated ~= target do
    local chosen, room = nil, -1
    for _, region in ipairs(regions) do
      local available = region.width - #region.label
      if available > room then chosen, room = region, available end
    end
    local step = allocated < target and 1 or -1
    chosen.width = chosen.width + step
    allocated = allocated + step
  end
end

function M.segment(used, regular, maximum, requested_width)
  if not integer(used, 0) then used = nil end
  local spans = { { text = used and count(used) or "?", fg = common.C.system } }
  local zones, capacity = regions(regular, maximum)
  if not capacity then
    local neutral = { text = string.rep(" ", 14) .. "?" .. string.rep(" ", 15),
      fg = common.C.md_code_fg, bg = "#30303C" }
    return { spans = { spans[1], { text = " " }, neutral,
      { text = " ?%", fg = common.C.system } },
      used = spans[1], bar = { neutral }, percent = "?" }
  end
  widths(zones, capacity, requested_width)
  spans[#spans + 1] = { text = " ", fg = common.C.system }
  local bar_width = 0
  for _, zone in ipairs(zones) do bar_width = bar_width + zone.width end
  local filled = used and math.min(bar_width, math.floor(bar_width * used / capacity + 0.5)) or 0
  local cell = 0
  for i, zone in ipairs(zones) do
    if i > 1 then
      spans[#spans + 1] = { text = "│", fg = "#D0D0D0", bg = "#181818" }
    end
    local left = math.floor((zone.width - #zone.label) / 2)
    local text = string.rep(" ", left) .. zone.label
      .. string.rep(" ", zone.width - left - #zone.label)
    for j = 1, zone.width do
      cell = cell + 1
      local active = cell <= filled
      local fg = not used and common.C.md_code_fg or (active and "#101010" or zone.color.label)
      local bg = not used and "#30303C" or (active and zone.color.filled or zone.color.empty)
      local char = text:sub(j, j)
      local previous = spans[#spans]
      if previous.fg == fg and previous.bg == bg then previous.text = previous.text .. char
      else spans[#spans + 1] = { text = char, fg = fg, bg = bg } end
    end
  end
  local percent = used and tostring(math.floor(100 * used / capacity + 0.5)) or "?"
  spans[#spans + 1] = { text = " " .. percent .. "%", fg = common.C.system }
  local bar = {}
  for i = 3, #spans - 1 do bar[#bar + 1] = spans[i] end
  return { spans = spans, used = spans[1], bar = bar, percent = percent }
end

return M
