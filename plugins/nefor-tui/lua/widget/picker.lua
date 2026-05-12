-- Generic selection widget. Operates on opaque entries supplied via a
-- zero-arg source function; all callbacks are data in opts.
--
-- Required state slice: { query = "...", cursor = 1 }
--
-- Data opts:
--   entries       zero-arg fn returning the candidate list
--   format_entry  fn(entry, opts) -> string|tui-tree (default tostring)
--   filter        fn(entries, query) -> filtered list (default = case-
--                  insensitive substring match against tostring(entry))
--   on_select     fn(entry, opts) called on Enter against a non-empty
--                  match list; return value is returned via handle()
--   on_cancel     fn() called on Esc
--
-- Visual opts:
--   title, title_style, cursor_style, row_style, search_style,
--   divider_style, empty_style
--   empty_text     shown when entries() is empty and query is ""
--   no_match_text  shown when entries() is non-empty but filter returns 0
--   cap            max visible rows around the cursor (default 12)
--   show_search    render the typed query as a row (default true)
--   footer         content rendered below the list
--
-- mount_in_popup(opts) returns the picker pre-wrapped in popup.view.

local util = require("nefor-tui.util")

local M = {}

local function default_filter(entries, query)
  if query == nil or query == "" then return entries or {} end
  local q = query:lower()
  local out = {}
  for _, e in ipairs(entries or {}) do
    local s = tostring(e):lower()
    if s:find(q, 1, true) ~= nil then out[#out + 1] = e end
  end
  return out
end

local function default_format(entry)
  return tostring(entry)
end

function M.filter(opts, entries, query)
  local fn = (opts and opts.filter) or default_filter
  return fn(entries or {}, query or "")
end

function M.clamp_cursor(cursor, n)
  if n == 0 then return 1 end
  if cursor == nil or cursor < 1 then return 1 end
  if cursor > n then return n end
  return cursor
end

function M.view(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("picker.view: opts must be a table, got " .. type(opts))
  end
  local state = opts.state or {}
  if type(state) ~= "table" then
    error("picker.view: opts.state must be a table, got " .. type(state))
  end
  local entries  = (opts.entries and opts.entries()) or {}
  local query    = state.query or ""
  local matches  = M.filter(opts, entries, query)
  local cursor   = M.clamp_cursor(state.cursor or 1, #matches)
  local format   = opts.format_entry or default_format
  local cap      = opts.cap or 12

  local children = {}
  if opts.title ~= nil then
    children[#children + 1] = tui.text {
      content = opts.title,
      style   = opts.title_style,
      wrap    = "none",
    }
  end
  if opts.show_search ~= false then
    children[#children + 1] = tui.text {
      content = "search: " .. query,
      style   = opts.search_style,
      wrap    = "none",
    }
    children[#children + 1] = tui.text {
      content = string.rep("─", 40),
      style   = opts.divider_style,
      wrap    = "none",
    }
  end

  local body = {}
  if #matches == 0 then
    if #entries == 0 then
      body[#body + 1] = tui.text {
        content = opts.empty_text or "(no entries)",
        style   = opts.empty_style,
        wrap    = "word",
      }
    else
      body[#body + 1] = tui.text {
        content = opts.no_match_text or "(no matches)",
        style   = opts.empty_style,
        wrap    = "none",
      }
    end
  else
    local first = 1
    if #matches > cap then
      first = math.max(1, math.min(cursor - cap + 1, #matches - cap + 1))
      if first < 1 then first = 1 end
    end
    local last = math.min(first + cap - 1, #matches)
    for i = first, last do
      local entry = matches[i]
      local rendered = format(entry, opts)
      local style = (i == cursor) and opts.cursor_style or opts.row_style
      if type(rendered) == "string" then
        body[#body + 1] = tui.text {
          content = rendered, style = style, wrap = "none",
        }
      elseif type(rendered) == "table" then
        body[#body + 1] = rendered
      else
        error("picker: format_entry must return string or table, got " .. type(rendered))
      end
    end
  end
  children[#children + 1] = tui.column { gap = 0, children = body }

  if opts.footer ~= nil then
    children[#children + 1] = util.resolve_content(opts.footer)
  end

  return tui.column { gap = opts.gap or 1, children = children }
end

function M.handle(opts, msg)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("picker.handle: opts must be a table, got " .. type(opts))
  end
  if opts.on_select ~= nil and type(opts.on_select) ~= "function" then
    error("picker.handle: opts.on_select must be a function, got " .. type(opts.on_select))
  end
  if opts.on_cancel ~= nil and type(opts.on_cancel) ~= "function" then
    error("picker.handle: opts.on_cancel must be a function, got " .. type(opts.on_cancel))
  end
  if msg == nil or msg.kind == nil then return nil end
  local kind = msg.kind
  local state    = opts.state or {}
  local entries  = (opts.entries and opts.entries()) or {}
  local query    = state.query or ""
  local matches  = M.filter(opts, entries, query)
  local cursor   = M.clamp_cursor(state.cursor or 1, #matches)

  if kind == "key.up" or kind == "key.down" then
    if #matches == 0 then return { state = {} } end
    local nxt = (kind == "key.up") and (cursor - 1) or (cursor + 1)
    if nxt < 1 then nxt = #matches end
    if nxt > #matches then nxt = 1 end
    return { state = { cursor = nxt } }
  end

  if kind == "key.enter" then
    if #matches == 0 then return { state = {} } end
    local entry = matches[cursor] or matches[1]
    if entry == nil then return { state = {} } end
    local result
    if opts.on_select ~= nil then
      result = opts.on_select(entry, opts)
    end
    return { state = {}, result = result, selected = entry }
  end

  if kind == "key.escape" then
    local result
    if opts.on_cancel ~= nil then
      result = opts.on_cancel()
    end
    return { state = {}, result = result, cancelled = true }
  end

  if opts.show_search ~= false then
    if kind == "key.backspace" then
      local q = query
      if #q > 0 then q = q:sub(1, #q - 1) end
      return { state = { query = q, cursor = 1 } }
    end
    if kind == "key.space" then
      return { state = { query = query .. " ", cursor = 1 } }
    end
    -- Printable single-char ASCII filter input.
    if kind:sub(1, 4) == "key." and #kind == 5 then
      local ch = kind:sub(5, 5)
      local b = string.byte(ch)
      if b and b >= 33 and b <= 126 then
        return { state = { query = query .. ch, cursor = 1 } }
      end
    end
  end

  return nil
end

local popup_mod = require("nefor-tui.widget.popup")
function M.mount_in_popup(opts)
  return popup_mod.view({
    open         = (opts and opts.state ~= nil),
    child        = M.view(opts),
    border_style = opts and opts.border_style,
    width        = opts and opts.width,
    height       = opts and opts.height,
    anchor       = opts and opts.anchor,
    scroll_key   = opts and opts.scroll_key,
    pad          = opts and opts.pad,
    gap          = 0,
  })
end

return M
