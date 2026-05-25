local M = {}

local height_cache = require("chat.height_cache")
local log = require("chat.log")

local function measure_width(key, padding)
  local ok, snap = pcall(tui.scroll_position, key)
  local lr = (padding.left or 0) + (padding.right or 0)
  if ok and snap and snap.inner_width and snap.inner_width > 0 then
    local mw = math.max(1, snap.inner_width - lr)
    log.log("vscroll", "width source=%s raw=%d after_padding=%d", "scroll_position", snap.inner_width, mw)
    return mw
  end
  local dims = tui.dimensions()
  local w = dims and dims.width or 80
  local mw = math.max(1, w - lr - 1)
  log.log("vscroll", "width source=%s raw=%d after_padding=%d", "fallback", w, mw)
  return mw
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
  local key = opts.key or "chat"
  local gap = opts.gap or 1
  local n = #entries

  local widgets = {}
  local vis

  local padding = opts.padding or { top = 0, right = 1, bottom = 0, left = 0 }

  if n > 0 then
    local mw = measure_width(key, padding)
    height_cache.set_width(mw)

    local heights = {}
    for i = 1, n do
      heights[i] = height_cache.get(entries[i], function(e)
        return opts.render_entry(e, i, opts.context)
      end)
    end

    local has_vsp = type(tui.virtual_scroll_prepare) == "function"
    vis = has_vsp and tui.virtual_scroll_prepare(key, n, heights, gap)
    if vis then
      log.log("vscroll", "prepare n=%d total_h=%d first=%d last=%d top_h=%d bot_h=%d",
        n, vis.total_h, vis.first, vis.last, vis.top_h, vis.bot_h)

      local visible = {}
      for i = vis.first, vis.last do
        if i >= 1 and i <= n then
          visible[#visible + 1] = tui.column {
            key = "e" .. i, gap = 0,
            children = { opts.render_entry(entries[i], i, opts.context) },
          }
        end
      end
      do
        local extra = opts.append
        if type(extra) == "function" then extra = extra() end
        visible[#visible + 1] = tui.column {
          key = "_append", gap = 0,
          children = extra and { extra } or {},
        }
      end
      -- Spacers in an outer gap=0 column so the column gap between
      -- entries doesn't add phantom rows at the spacer boundaries.
      -- The inner column carries the real gap between entries.
      widgets[#widgets + 1] = tui.constrained {
        key = "_top", min_height = vis.top_h, max_height = vis.top_h,
        child = tui.text { content = "" },
      }
      widgets[#widgets + 1] = tui.column { key = "_vis", gap = gap, children = visible }
      widgets[#widgets + 1] = tui.constrained {
        key = "_bot", min_height = vis.bot_h, max_height = vis.bot_h,
        child = tui.text { content = "" },
      }
    else
      for i = 1, n do
        widgets[#widgets + 1] = tui.column {
          key = "e" .. i, gap = 0,
          children = { opts.render_entry(entries[i], i, opts.context) },
        }
      end
      do
        local extra = opts.append
        if type(extra) == "function" then extra = extra() end
        widgets[#widgets + 1] = tui.column {
          key = "_append", gap = 0,
          children = extra and { extra } or {},
        }
      end
    end
  else
    local extra = opts.append
    if type(extra) == "function" then extra = extra() end
    widgets[#widgets + 1] = tui.column {
      key = "_append", gap = 0,
      children = extra and { extra } or {},
    }
  end

  -- When virtual scroll is active, the outer column uses gap=0 so
  -- spacers sit flush against the content subcolumn. Entry gaps live
  -- inside the _vis subcolumn. Without virtual scroll, the outer
  -- column uses the normal gap between entries.
  local outer_gap = vis and 0 or gap

  local scroll_widget = tui.scrollable {
    key                    = key,
    stick_to               = opts.stick_to ~= nil and opts.stick_to or "end",
    scrollbar              = "auto",
    selectable             = opts.selectable ~= false,
    virtual_content_height = vis and vis.total_h or nil,
    child                  = tui.padding {
      value = padding,
      child = tui.column { gap = outer_gap, children = widgets },
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
