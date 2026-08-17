local usage = require("libs.chat.usage")

local real_time = os.time
local now = real_time { year = 2026, month = 6, day = 15, hour = 12, min = 0, sec = 0 }
os.time = function() return now end

local function subscription(reset_at)
  return {
    kind = "subscription",
    windows = { { used_percent = 33, reset_at = reset_at } },
  }
end

local same_day = real_time { year = 2026, month = 6, day = 15, hour = 17, min = 21, sec = 0 }
assert(usage.footer(subscription(same_day)) == "◕ 67% until 17:21")

local future_date = real_time { year = 2026, month = 6, day = 22, hour = 17, min = 21, sec = 0 }
assert(usage.footer(subscription(future_date)) == "◕ 67% until Jun 22 17:21")

local future_year = real_time { year = 2027, month = 1, day = 3, hour = 8, min = 5, sec = 0 }
assert(usage.footer(subscription(future_year)) == "◕ 67% until Jan 3, 2027 08:05")

assert(usage.footer({ kind = "monetary", amount = "0.01234", currency = "USD" }) == "$0.01234")
assert(usage.footer({ kind = "free" }) == "free")
assert(usage.footer({ kind = "unknown" }) == nil)
assert(usage.markdown_value({ kind = "free" }) == "**Free**")
assert(usage.markdown_value({ kind = "unknown" }) == "Usage data is unavailable.")
