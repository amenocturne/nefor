-- Generic text / tui-tree display pane. Scrollable container for
-- free-form content (sidebars, debug output, statusline-adjacent panels).
--
-- Opts:
--   content     string, tui tree, or zero-arg fn returning either
--   style       applied when content is a raw string
--   wrap        "word" (default) or "none" (string content only)
--   scrollable  wrap content in tui.scrollable (default true)
--   key         scrollable key (default "text-pane")
--   selectable  drag-select participation when scrollable (default true)
--   padding     applied around the content

local M = {}

local function resolve(opts)
  local c = opts.content
  if type(c) == "function" then c = c() end
  if c == nil then return nil end
  if type(c) == "string" then
    return tui.text {
      content = c,
      style   = opts.style,
      wrap    = opts.wrap or "word",
    }
  end
  if type(c) == "table" then
    return c
  end
  error("text_pane: content must be nil, string, function, or table; got " .. type(c))
end

function M.view(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("text_pane.view: opts must be a table, got " .. type(opts))
  end
  local body = resolve(opts)
  if body == nil then return nil end
  if opts.padding ~= nil then
    body = tui.padding { value = opts.padding, child = body }
  end
  if opts.scrollable == false then return body end
  return tui.scrollable {
    key        = opts.key or "text-pane",
    scrollbar  = "auto",
    selectable = opts.selectable ~= false,
    child      = body,
  }
end

function M.handle(opts, msg)
  opts = opts or {}
  if msg == nil or msg.kind == nil then return nil end
  if opts.scrollable == false then return nil end
  local key = opts.key or "text-pane"
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

return M
