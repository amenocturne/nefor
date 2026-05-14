-- Bordered overlay widget. Floats above the main layout via tui.anchored.
-- View is render-only; the widget owns no state — the caller holds an
-- `open` boolean and gates view + handle on it.
--
-- Layout opts:
--   child         inner content (tui tree, string, or function)
--   title         optional string row at top
--   border_style  { fg, bold?, italic? }
--   title_style   { fg, bold? }
--   width, height anchored sizing (e.g. "60%", 80, nil)
--   anchor        "center" (default), "top", "bottom", ...
--   scroll_key    inner scrollable key (default "popup")
--   pad, gap      inner padding (default 1) and column gap (default 1)
--   footer        optional content row below the body
--
-- Event opts:
--   keys          { ["key.escape"] = handler, ... }; each handler is
--                  called with an `api` table whose `api.close()`
--                  returns `{ open = false }` for the caller to fold
--                  back into state.

local util = require("nefor-tui.util")

local M = {}

local function build_api()
  return {
    close = function() return { open = false } end,
  }
end

function M.view(opts)
  if not opts or opts.open == false then return nil end
  if type(opts) ~= "table" then
    error("popup.view: opts must be a table, got " .. type(opts))
  end
  local children = {}
  if opts.title ~= nil then
    if type(opts.title) ~= "string" then
      error("popup.view: opts.title must be a string, got " .. type(opts.title))
    end
    children[#children + 1] = tui.text {
      content = opts.title,
      style   = opts.title_style,
      wrap    = "none",
    }
  end
  local child = util.resolve_content(opts.child)
  if child ~= nil then children[#children + 1] = child end
  if opts.footer ~= nil then
    children[#children + 1] = util.resolve_content(opts.footer)
  end

  local pad = opts.pad
  if pad == nil then pad = 1 end
  local gap = opts.gap
  if gap == nil then gap = 1 end
  local body = tui.padding {
    value = pad,
    child = tui.column { gap = gap, children = children },
  }

  return tui.anchored {
    anchor   = opts.anchor or "center",
    width    = opts.width,
    height   = opts.height,
    offset_x = opts.offset_x,
    offset_y = opts.offset_y,
    child    = util.bordered_popup_shell(
      opts.scroll_key or "popup",
      body,
      opts.border_style
    ),
  }
end

-- Dispatch a message against opts.keys. Returns the handler's return
-- value or nil if no handler matched.
function M.handle(opts, msg)
  if not opts or opts.open == false then return nil end
  if msg == nil or msg.kind == nil then return nil end
  local keys = opts.keys
  if keys == nil then return nil end
  if type(keys) ~= "table" then
    error("popup.handle: opts.keys must be a table, got " .. type(keys))
  end
  local fn = keys[msg.kind]
  if fn == nil then return nil end
  if type(fn) ~= "function" then
    error("popup.handle: keys[" .. tostring(msg.kind) .. "] must be a function, got " .. type(fn))
  end
  return fn(build_api(), msg)
end

function M.is_open(opts)
  return opts ~= nil and opts.open ~= false and opts.open ~= nil
end

return M
