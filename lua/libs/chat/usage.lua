local M = {}

local function clamp_percent(value)
  local n = tonumber(value)
  if n == nil then return nil end
  if n < 0 then return 0 end
  if n > 100 then return 100 end
  return math.floor(n + 0.5)
end

function M.available_percent(window)
  if type(window) ~= "table" then return nil end
  local used = clamp_percent(window.used_percent)
  return used and (100 - used) or nil
end

function M.gauge(available)
  local pct = clamp_percent(available)
  if pct == nil then return nil end
  if pct >= 88 then return "●" end
  if pct >= 63 then return "◕" end
  if pct >= 38 then return "◑" end
  if pct >= 13 then return "◔" end
  return "○"
end

local MONTHS = { "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec" }

function M.reset_time(window, now)
  local timestamp = type(window) == "table" and tonumber(window.reset_at) or nil
  if timestamp == nil or timestamp <= 0 then return nil end
  local reset, current = os.date("*t", timestamp), os.date("*t", tonumber(now) or os.time())
  local time = string.format("%02d:%02d", reset.hour, reset.min)
  if reset.year == current.year and reset.yday == current.yday then return time end
  local date = string.format("%s %d", MONTHS[reset.month], reset.day)
  return reset.year == current.year and (date .. " " .. time)
    or string.format("%s, %d %s", date, reset.year, time)
end

function M.primary(snapshot)
  if type(snapshot) ~= "table" then return nil end
  if snapshot.kind == "subscription" then return (snapshot.windows or {})[1] end
  local limit = snapshot.rate_limit
  return type(limit) == "table" and limit.primary_window or nil
end

function M.footer(snapshot)
  if type(snapshot) ~= "table" then return nil end
  if snapshot.kind == "monetary" and type(snapshot.amount) == "string"
      and snapshot.amount:match("^%d+%.?%d*$") then
    if snapshot.currency == "USD" then return "$" .. tostring(snapshot.amount) end
    return tostring(snapshot.amount) .. " " .. tostring(snapshot.currency or "")
  end
  if snapshot.kind == "free" then return "free" end
  if snapshot.kind ~= nil and snapshot.kind ~= "subscription" then return nil end
  local available = M.available_percent(M.primary(snapshot))
  if available == nil then return nil end
  local text = string.format("%s %d%%", M.gauge(available), available)
  local reset = M.reset_time(M.primary(snapshot))
  if reset then text = text .. " until " .. reset end
  return text, available
end

local function window_title(window, fallback)
  local seconds = type(window) == "table" and tonumber(window.window_seconds or window.limit_window_seconds) or nil
  if seconds == 18000 then return "5-hour window" end
  if seconds == 604800 then return "Weekly window" end
  if seconds and seconds > 0 and seconds % 3600 == 0 then return tostring(seconds / 3600) .. "-hour window" end
  return fallback
end

local function window_markdown(window, fallback)
  local used, available = clamp_percent(window and window.used_percent), M.available_percent(window)
  if used == nil or available == nil then return nil end
  local lines = { "**" .. window_title(window, fallback) .. "**",
    string.format("%d%% used · %d%% available", used, available) }
  local reset = M.reset_time(window)
  if reset then lines[#lines + 1] = "Resets at " .. reset end
  return table.concat(lines, "\n")
end

-- Provider constants are rendered independently; this module never folds
-- providers or currencies.
function M.markdown_value(value)
  if type(value) ~= "table" then return "Usage data is unavailable." end
  if value.kind == "monetary" then
    local text = select(1, M.footer(value))
    return text and ("**Cost**\n" .. text) or "Usage data is unavailable."
  elseif value.kind == "free" then return "**Free**"
  elseif value.kind == "unknown" then return "Usage data is unavailable."
  end
  local sections = {}
  local windows = value.kind == "subscription" and value.windows or {
    value.rate_limit and value.rate_limit.primary_window,
    value.rate_limit and value.rate_limit.secondary_window,
  }
  for index, window in ipairs(windows or {}) do
    local section = window_markdown(window, index == 1 and "Primary window" or "Secondary window")
    if section then sections[#sections + 1] = section end
  end
  local plan = value.plan or value.plan_type
  if type(plan) == "string" and plan ~= "" then sections[#sections + 1] = "Plan: `" .. plan .. "`" end
  if #sections == 0 then return "Usage data is unavailable." end
  return table.concat(sections, "\n\n")
end

function M.markdown(snapshot)
  if type(snapshot) ~= "table" then return "Usage data is unavailable." end
  if snapshot.usage_id then return M.markdown_value(snapshot.usage) end
  local sections = {}
  for _, value in ipairs(snapshot) do
    if type(value) == "table" and type(value.usage) == "table"
        and value.usage.kind ~= "unknown" then
      sections[#sections + 1] = "### " .. value.usage_id .. "\n\n" .. M.markdown_value(value.usage)
    end
  end
  if #sections > 0 then return table.concat(sections, "\n\n") end
  if #snapshot > 0 then return "Usage data is unavailable." end
  return M.markdown_value(snapshot)
end

return M
