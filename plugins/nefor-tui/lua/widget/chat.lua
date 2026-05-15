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

-- Per-key height caches. Maps entry index → estimated row count.
-- Entries outside the visible window are replaced with a spacer of
-- this height, avoiding render_entry + Rust layout/paint work.
local height_caches = {}

local function estimate_height(entry)
  local text = entry.text or ""
  local len = #text
  if len == 0 then return 2 end
  -- Rough heuristic: ~80 chars per wrapped line + overhead for
  -- markdown structure, code blocks, separators.
  local lines = math.ceil(len / 80)
  if entry.kind == "tool_start" or entry.kind == "tool_end" then
    return math.max(3, lines)
  end
  return math.max(2, lines + 2)
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

  local hcache = height_caches[key]
  if not hcache then hcache = {}; height_caches[key] = hcache end

  -- Trim stale cache entries when the entry list shrinks (e.g. /new).
  local n = #entries
  for idx = n + 1, #hcache do hcache[idx] = nil end

  -- Two-pass viewport windowing: first pass computes cumulative y
  -- positions and identifies the visible entry range. Second pass emits
  -- at most 3 children: a top spacer (one widget for all above-viewport
  -- entries), the visible entries, and a bottom spacer. This keeps the
  -- column's child count at O(viewport) instead of O(total_entries),
  -- so the Rust reconciler + flex_layout stay fast regardless of
  -- transcript length.
  local buffer = viewport_h * 2
  local vis_top = scroll_y - buffer
  local vis_bot = scroll_y + viewport_h + buffer

  local heights = {}
  local cumul   = {}
  local y = 0
  for i = 1, n do
    local h = hcache[i] or estimate_height(entries[i])
    if not hcache[i] then hcache[i] = h end
    heights[i] = h
    cumul[i]   = y
    y = y + h + gap
  end

  local first_vis, last_vis = n + 1, 0
  for i = 1, n do
    local entry_top = cumul[i]
    local entry_bot = entry_top + heights[i]
    if entry_bot >= vis_top and entry_top <= vis_bot then
      if i < first_vis then first_vis = i end
      if i > last_vis  then last_vis  = i end
    end
  end

  local widgets = {}

  -- Top spacer: total height of all entries above the visible window.
  if first_vis > 1 then
    local top_h = cumul[first_vis]
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
  if last_vis < n then
    local bot_start = cumul[last_vis] + heights[last_vis] + gap
    local total_h   = cumul[n] + heights[n]
    local bot_h     = total_h - bot_start
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
  height_caches[key_name or "chat"] = {}
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
