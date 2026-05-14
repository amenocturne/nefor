local M = {}

-- Sentinel for "clear this key" inside a merge patch. Lua's `pairs` skips
-- keys whose value is nil, so a merge can't otherwise distinguish "leave
-- alone" from "unset"; an explicit marker is the workaround.
M.NIL = {}

-- Copy all keys from `a`, then overwrite with keys from `b`; values
-- equal to `M.NIL` clear the key. Either arg may be nil.
function M.shallow_merge(a, b)
  local out = {}
  if a ~= nil then
    for k, v in pairs(a) do out[k] = v end
  end
  if b ~= nil then
    for k, v in pairs(b) do
      if v == M.NIL then out[k] = nil else out[k] = v end
    end
  end
  return out
end

-- Resolve a content slot for a widget. Three accepted shapes:
--   string   -> wrapped in tui.text with the given style
--   function -> invoked with no args; result re-resolved
--   table    -> assumed to already be a tui-tree, passed through
-- nil short-circuits to nil. Anything else is a usage bug — error with
-- a message that names the offending type so the user can correct it.
function M.resolve_content(content, style)
  if content == nil then return nil end
  if type(content) == "function" then
    return M.resolve_content(content(), style)
  end
  if type(content) == "string" then
    return tui.text { content = content, style = style, wrap = "word" }
  end
  if type(content) == "table" then
    return content
  end
  error("resolve_content: unsupported content type " .. type(content)
        .. " (expected nil, string, function, or table)")
end

-- Bordered box around `child`. `key` stamps the outer column so the
-- reconciler can preserve the same instance across layout shifts (e.g.
-- when an autocomplete row opens above an input and pushes the input's
-- position in its parent column).
function M.bordered_box(child, border_style, key)
  local function rule_row(left, right)
    return tui.constrained {
      max_height = 1,
      child = tui.row {
        gap = 0,
        children = {
          tui.text { content = left,  style = border_style, wrap = "none" },
          tui.expanded { child = tui.fill { char = "─", style = border_style } },
          tui.text { content = right, style = border_style, wrap = "none" },
        },
      },
    }
  end
  local side_bar = tui.constrained {
    max_width = 1,
    child = tui.fill { char = "│", style = border_style },
  }
  local body_row = tui.row {
    gap = 0,
    children = {
      side_bar,
      tui.expanded {
        child = tui.padding {
          left = 1, right = 1, top = 0, bottom = 0, child = child,
        },
      },
      side_bar,
    },
  }
  return tui.column {
    gap = 0,
    key = key,
    children = { rule_row("╭", "╮"), body_row, rule_row("╰", "╯") },
  }
end

-- Variant of `bordered_box` for popup shells: body uses `tui.expanded`
-- so the bottom rule paints even when content exceeds the allotted
-- height, wraps the body in `tui.scrollable` for overflow, and stacks
-- an opaque `tui.fill { char = " " }` behind it so transcript content
-- doesn't bleed through empty rows.
function M.bordered_popup_shell(scroll_key, child, border_style)
  local function rule_row(left, right)
    return tui.constrained {
      max_height = 1,
      child = tui.row {
        gap = 0,
        children = {
          tui.text { content = left,  style = border_style, wrap = "none" },
          tui.expanded { child = tui.fill { char = "─", style = border_style } },
          tui.text { content = right, style = border_style, wrap = "none" },
        },
      },
    }
  end
  local side_bar = tui.constrained {
    max_width = 1,
    child = tui.fill { char = "│", style = border_style },
  }
  local body_bg = tui.fill { char = " " }
  local body_row = tui.row {
    gap = 0,
    children = {
      side_bar,
      tui.expanded {
        child = tui.stack {
          children = {
            body_bg,
            tui.padding {
              left = 1, right = 1, top = 0, bottom = 0,
              child = tui.scrollable {
                key       = scroll_key,
                scrollbar = "auto",
                child     = child,
              },
            },
          },
        },
      },
      side_bar,
    },
  }
  return tui.column {
    gap = 0,
    children = {
      rule_row("╭", "╮"),
      tui.expanded { child = body_row },
      rule_row("╰", "╯"),
    },
  }
end

return M
