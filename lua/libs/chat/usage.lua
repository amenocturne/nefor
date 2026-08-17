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
  if used == nil then return nil end
  return 100 - used
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
  if type(window) ~= "table" then return nil end
  local timestamp = tonumber(window.reset_at)
  if timestamp == nil or timestamp <= 0 then return nil end
  local reset = os.date("*t", timestamp)
  local current = os.date("*t", tonumber(now) or os.time())
  local time = string.format("%02d:%02d", reset.hour, reset.min)
  if reset.year == current.year and reset.yday == current.yday then return time end
  local date = string.format("%s %d", MONTHS[reset.month], reset.day)
  if reset.year == current.year then return date .. " " .. time end
  return string.format("%s, %d %s", date, reset.year, time)
end

function M.primary(snapshot)
  local limit = type(snapshot) == "table" and snapshot.rate_limit or nil
  return type(limit) == "table" and limit.primary_window or nil
end

function M.footer(snapshot)
  local window = M.primary(snapshot)
  local available = M.available_percent(window)
  if available == nil then return nil end
  local text = string.format("%s %d%%", M.gauge(available), available)
  local reset = M.reset_time(window)
  if reset ~= nil then text = text .. " until " .. reset end
  return text, available
end

local function window_title(window, fallback)
  local seconds = type(window) == "table" and tonumber(window.limit_window_seconds) or nil
  if seconds == 18000 then return "5-hour window" end
  if seconds == 604800 then return "Weekly window" end
  if seconds ~= nil and seconds > 0 and seconds % 3600 == 0 then
    return tostring(seconds / 3600) .. "-hour window"
  end
  return fallback
end

local function window_markdown(window, fallback)
  if type(window) ~= "table" then return nil end
  local used = clamp_percent(window.used_percent)
  local available = M.available_percent(window)
  if used == nil or available == nil then return nil end
  local lines = {
    "**" .. window_title(window, fallback) .. "**",
    string.format("%d%% used · %d%% available", used, available),
  }
  local reset = M.reset_time(window)
  if reset ~= nil then lines[#lines + 1] = "Resets at " .. reset end
  return table.concat(lines, "\n")
end

function M.markdown(snapshot)
  if type(snapshot) ~= "table" then return "Usage data is unavailable." end
  local limit = snapshot.rate_limit or {}
  local sections = {}
  local primary = window_markdown(limit.primary_window, "Primary window")
  local secondary = window_markdown(limit.secondary_window, "Secondary window")
  if primary ~= nil then sections[#sections + 1] = primary end
  if secondary ~= nil then sections[#sections + 1] = secondary end
  if type(snapshot.plan_type) == "string" and snapshot.plan_type ~= "" then
    sections[#sections + 1] = "Plan: `" .. snapshot.plan_type .. "`"
  end
  local credits = snapshot.credits
  if type(credits) == "table" then
    if credits.unlimited == true then
      sections[#sections + 1] = "Credits: unlimited"
    elseif credits.balance ~= nil then
      sections[#sections + 1] = "Credits: " .. tostring(credits.balance)
    end
  end
  if #sections == 0 then return "Usage data is unavailable." end
  return table.concat(sections, "\n\n")
end

return M
