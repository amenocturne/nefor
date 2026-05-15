-- Scrollable message transcript widget. Generic over entry shape: the
-- caller supplies the entry list (via zero-arg `entries`) and a
-- per-entry renderer.
--
-- Opts:
--   entries       zero-arg fn returning the current entry list
--   render_entry  fn(entry, index, ctx) -> tui tree (required)
--   context       opaque table forwarded to render_entry's third arg
--   key           scrollable key (default "chat")
--   gap           vertical gap between entries (default 1)
--   stick_to      "end" (default — auto-follow on new entries) or nil
--   selectable    drag-to-select participation (default true)
--   empty_view    tui tree or zero-arg fn — rendered when entries is empty
--   append        tui tree or zero-arg fn — inserted after last entry,
--                  inside the scrollable (e.g. streaming indicator)
--   padding       { top, right, bottom, left } around inner column

local M = {}

-- Per-key layout geometry cache. Holds per-entry heights, cumulative
-- y positions, and total content height. Incrementally updated: when
-- entries are appended (normal chat flow), only the new tail is
-- computed. Full recompute only on entry count shrink (/new, /resume).
local geo_caches = {}

local function estimate_height(entry)
  local text = entry.text or ""
  local len = #text
  if len == 0 then return 2 end
  local lines = math.ceil(len / 80)
  if entry.kind == "tool_start" or entry.kind == "tool_end" then
    return math.max(3, lines)
  end
  return math.max(2, lines + 2)
end

local function get_geo(key, entries, gap)
  local n = #entries
  local gc = geo_caches[key]
  if not gc then
    gc = { heights = {}, cumul = {}, total = 0, count = 0 }
    geo_caches[key] = gc
  end
  -- Shrink: full recompute.
  if n < gc.count then
    gc.heights = {}; gc.cumul = {}; gc.total = 0; gc.count = 0
  end
  -- Incremental: compute only new entries from gc.count+1..n.
  local start = gc.count + 1
  local y = gc.total
  for i = start, n do
    if i > 1 and i == start then y = y + gap end
    local h = gc.heights[i] or estimate_height(entries[i])
    gc.heights[i] = h
    gc.cumul[i]   = y
    y = y + h
    if i < n then y = y + gap end
  end
  gc.total = y
  gc.count = n
  return gc
end

function M.view(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("chat.view: opts must be a table, got " .. type(opts))
  end
  if type(opts.render_entry) ~= "function" then
    error("chat.view: opts.render_entry is required (must be a function), got " .. type(opts.render_entry))
  end
  local entries = (opts.entries and opts.entries()) or {}
  local ctx = opts.context
  local key = opts.key or "chat"
  local gap = opts.gap or 1

  local scroll_y, viewport_h = 0, 100
  local ok, snap = pcall(tui.scroll_position, key)
  if ok and snap then
    scroll_y   = snap.offset or 0
    viewport_h = snap.viewport_size or 100
  end

  local n = #entries
  local gc = get_geo(key, entries, gap)

  -- Binary search for the first entry whose bottom edge >= vis_top.
  local buffer = viewport_h * 2
  local vis_top = scroll_y - buffer
  local vis_bot = scroll_y + viewport_h + buffer

  local first_vis, last_vis = n + 1, 0
  -- Binary search: first entry with bottom edge >= vis_top.
  local lo, hi = 1, n
  while lo <= hi do
    local mid = math.floor((lo + hi) / 2)
    local bot = gc.cumul[mid] + gc.heights[mid]
    if bot < vis_top then lo = mid + 1 else hi = mid - 1 end
  end
  first_vis = lo
  -- Binary search: last entry with top edge <= vis_bot.
  lo, hi = first_vis, n
  while lo <= hi do
    local mid = math.floor((lo + hi) / 2)
    if gc.cumul[mid] <= vis_bot then lo = mid + 1 else hi = mid - 1 end
  end
  last_vis = hi

  local widgets = {}

  -- Top spacer: total height of all entries above the visible window.
  if first_vis > 1 and first_vis <= n then
    local top_h = gc.cumul[first_vis]
    if top_h > 0 then
      widgets[#widgets + 1] = tui.constrained {
        key = "_top",
        min_height = top_h,
        max_height = top_h,
        child = tui.text { content = "" },
      }
    end
  end

  -- Visible entries: fully rendered.
  for i = first_vis, last_vis do
    if i >= 1 and i <= n then
      widgets[#widgets + 1] = opts.render_entry(entries[i], i, ctx)
    end
  end

  -- Bottom spacer: total height of all entries below the visible window.
  if last_vis >= 1 and last_vis < n then
    local bot_start = gc.cumul[last_vis] + gc.heights[last_vis] + gap
    local bot_h     = gc.total - bot_start
    if bot_h > 0 then
      widgets[#widgets + 1] = tui.constrained {
        key = "_bot",
        min_height = bot_h,
        max_height = bot_h,
        child = tui.text { content = "" },
      }
    end
  end

  if opts.append ~= nil then
    local extra = opts.append
    if type(extra) == "function" then extra = extra() end
    if extra ~= nil then widgets[#widgets + 1] = extra end
  end

  local padding = opts.padding or { top = 0, right = 1, bottom = 0, left = 0 }
  local scroll_widget = tui.scrollable {
    key        = key,
    stick_to   = opts.stick_to ~= nil and opts.stick_to or "end",
    scrollbar  = "auto",
    selectable = opts.selectable ~= false,
    child      = tui.padding {
      value = padding,
      child = tui.column { gap = gap, children = widgets },
    },
  }

  if #entries == 0 and opts.append == nil and opts.empty_view ~= nil then
    local empty = opts.empty_view
    if type(empty) == "function" then empty = empty() end
    if empty ~= nil then
      return tui.stack { children = { scroll_widget, empty } }
    end
  end
  return scroll_widget
end

function M.clear_heights(key_name)
  geo_caches[key_name or "chat"] = nil
end

-- Route scroll keys to tui.scroll_* against the widget's key. Caller
-- decides which messages reach this handler. Returns `{ state = {} }`
-- when consumed, nil otherwise.
function M.handle(opts, msg)
  opts = opts or {}
  if msg == nil or msg.kind == nil then return nil end
  local key = opts.key or "chat"
  local kind = msg.kind
  if kind == "key.pageup" then
    tui.scroll_by(key, -10); return { state = {} }
  end
  if kind == "key.pagedown" then
    tui.scroll_by(key, 10); return { state = {} }
  end
  if kind == "key.up" then
    tui.scroll_by(key, -1); return { state = {} }
  end
  if kind == "key.down" then
    tui.scroll_by(key, 1); return { state = {} }
  end
  if kind == "key.home" then
    tui.scroll_to(key, 0); return { state = {} }
  end
  if kind == "key.end" then
    tui.scroll_into_view(key); return { state = {} }
  end
  return nil
end

function M.scroll_to_end(opts)
  tui.scroll_into_view((opts and opts.key) or "chat")
end

function M.position(opts)
  local key = (opts and opts.key) or "chat"
  local ok, snap = pcall(tui.scroll_position, key)
  if not ok then return nil end
  return snap
end

return M
