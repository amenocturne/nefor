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

  if n > 0 then
    -- Build heights array for the Rust geometry engine.
    local heights = {}
    for i = 1, n do
      heights[i] = estimate_height(entries[i])
    end

    local has_vsp = type(tui.virtual_scroll_prepare) == "function"
    local vis = has_vsp and tui.virtual_scroll_prepare(key, n, heights, gap)
    if vis then
      -- Top spacer (always present to keep column child count stable).
      widgets[#widgets + 1] = tui.constrained {
        key = "_top", min_height = vis.top_h, max_height = vis.top_h,
        child = tui.text { content = "" },
      }
      -- Visible entries.
      for i = vis.first, vis.last do
        if i >= 1 and i <= n then
          widgets[#widgets + 1] = opts.render_entry(entries[i], i, opts.context)
        end
      end
      -- Bottom spacer (always present to keep column child count stable).
      widgets[#widgets + 1] = tui.constrained {
        key = "_bot", min_height = vis.bot_h, max_height = vis.bot_h,
        child = tui.text { content = "" },
      }
    else
      -- First frame (no scroll position yet): render all.
      for i = 1, n do
        widgets[#widgets + 1] = opts.render_entry(entries[i], i, opts.context)
      end
    end
  end

  if opts.append ~= nil then
    local extra = opts.append
    if type(extra) == "function" then extra = extra() end
    if extra ~= nil then widgets[#widgets + 1] = extra end
  end

  local padding = opts.padding or { top = 0, right = 1, bottom = 0, left = 0 }
  local scroll_widget = tui.scrollable {
    key                    = key,
    stick_to               = opts.stick_to ~= nil and opts.stick_to or "end",
    scrollbar              = "auto",
    selectable             = opts.selectable ~= false,
    virtual_content_height = vis and vis.total_h or nil,
    child                  = tui.padding {
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
  local key = key_name or "chat"
  if type(tui.virtual_scroll_invalidate) == "function" then
    tui.virtual_scroll_invalidate(key)
  end
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
