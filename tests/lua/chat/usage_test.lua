local usage = require("libs.chat.usage")

local real_time = os.time
local now = real_time { year = 2026, month = 6, day = 15, hour = 12, min = 0, sec = 0 }
os.time = function() return now end

local function snapshot(reset_at)
  return {
    rate_limit = {
      primary_window = {
        used_percent = 33,
        reset_at = reset_at,
      },
    },
  }
end

local same_day = real_time { year = 2026, month = 6, day = 15, hour = 17, min = 21, sec = 0 }
assert(usage.footer(snapshot(same_day)) == "◕ 67% until 17:21")

local future_date = real_time { year = 2026, month = 6, day = 22, hour = 17, min = 21, sec = 0 }
assert(usage.footer(snapshot(future_date)) == "◕ 67% until Jun 22 17:21")

local future_year = real_time { year = 2027, month = 1, day = 3, hour = 8, min = 5, sec = 0 }
assert(usage.footer(snapshot(future_year)) == "◕ 67% until Jan 3, 2027 08:05")
