local M = {}

local log = require("chat.log")
local cache = {}
local cache_size = 0
local current_width = nil
local MAX_CACHE_SIZE = 4096

function M.set_width(w)
  if w ~= current_width then
    if current_width ~= nil then
      log.log("cache", "width_changed %d -> %d, invalidating %d entries", current_width, w, cache_size)
    end
    cache = {}
    cache_size = 0
    current_width = w
  end
end

function M.get(entry, render_fn)
  local v = entry and entry.v
  if type(v) ~= "number" and type(v) ~= "string" then
    return tui.measure(render_fn(entry), current_width)
  end
  local cached = cache[v]
  if cached then
    log.log("cache", "hit v=%s h=%d", tostring(v), cached)
    return cached
  end
  if cache_size >= MAX_CACHE_SIZE then
    log.log("cache", "evict: cap reached (%d), full clear", cache_size)
    cache = {}
    cache_size = 0
  end
  local h = tui.measure(render_fn(entry), current_width)
  cache[v] = h
  cache_size = cache_size + 1
  log.log("cache", "miss v=%s w=%s -> h=%d (size=%d)", tostring(v), tostring(current_width), h, cache_size)
  return h
end

function M.invalidate_all()
  log.log("cache", "invalidate_all (%d entries)", cache_size)
  cache = {}
  cache_size = 0
end

function M.stats()
  return { size = cache_size, width = current_width }
end

return M
