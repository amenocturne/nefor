-- Ephemeral, non-blocking notification pill. Auto-dismisses on
-- `started_at_ms + ttl_ms`. Stacks behind popups by render order;
-- caller decides z-order by where it places `toast.view` in its
-- `tui.stack`.
--
-- State contract (held by the caller's reducer):
--   toasts = { { id, text, level, started_at_ms, ttl_ms }, ... }
-- level ∈ "info" | "warn" | "error". The caller is responsible for
-- dropping expired entries — see `toast.is_expired(entry, now_ms)`.
--
-- view opts:
--   toasts   list of entries (required)
--   now_ms   current frame clock (defaults to tui.now_ms())
-- returns the snabbdom node for the oldest non-expired toast, or nil.
-- Multiple simultaneous toasts aren't rendered side-by-side — the
-- oldest wins until it expires. The chat surface today only ever
-- holds one toast at a time; the list-shaped contract keeps the
-- door open without committing to a stacking layout.

local M = {}

local PALETTE = {
  info  = { fg = "#88ccff", border = { fg = "#7faaaa" } },
  warn  = { fg = "#D7AF5F", border = { fg = "#D7AF5F" } },
  error = { fg = "#D75F5F", border = { fg = "#D75F5F" } },
}

local ENTER_MS      = 220
local EXIT_MS       = 220
-- Extra dashes past the right edge so the rules visually continue
-- off-screen rather than terminating exactly at the edge.
local RIGHT_OVERFLOW = 6
local FULL_HEIGHT    = 3

local function clamp01(t)
  if t < 0 then return 0 end
  if t > 1 then return 1 end
  return t
end

local function ease_out_cubic(t)
  t = clamp01(t)
  local u = 1 - t
  return 1 - u * u * u
end

local function ease_in_cubic(t)
  t = clamp01(t)
  return t * t * t
end

local function palette_for(level)
  -- Unknown levels fall back to info so a malformed entry doesn't
  -- crash render — the caller wires from data, not a typed enum.
  return PALETTE[level] or PALETTE.info
end

function M.is_expired(entry, now_ms)
  if entry == nil then return true end
  local expires = (entry.started_at_ms or 0) + (entry.ttl_ms or 0)
  return now_ms >= expires
end

function M.view(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("toast.view: opts must be a table, got " .. type(opts))
  end
  local toasts = opts.toasts or {}
  if #toasts == 0 then return nil end
  local now = opts.now_ms or tui.now_ms()
  -- Oldest non-expired wins. The reducer should be pruning expired
  -- entries on every update; this filter is defence-in-depth so a
  -- caller that forgets one tick doesn't render a stuck toast.
  local active
  for _, t in ipairs(toasts) do
    if not M.is_expired(t, now) then
      active = t
      break
    end
  end
  if active == nil then return nil end

  local started = active.started_at_ms or now
  local ttl     = active.ttl_ms or 2000
  local expires = started + ttl
  local elapsed   = now - started
  local time_left = expires - now

  local text = active.text or ""
  local pal  = palette_for(active.level)

  -- Pill at rest: leading chrome + space + text + RIGHT_OVERFLOW
  -- cells of dashes that get clipped past the window's right edge.
  -- The slide is implemented by varying the anchored rect's WIDTH
  -- from 0 (off-screen) to pill_w_at_rest. Because the pill anchors
  -- bottom-right, the rect grows leftward as visible_w increases.
  local pill_w_at_rest = 2 + #text + RIGHT_OVERFLOW
  local total_slide    = pill_w_at_rest
  local distance_slid
  if elapsed < ENTER_MS then
    distance_slid = total_slide * ease_out_cubic(elapsed / ENTER_MS)
  elseif time_left < EXIT_MS then
    distance_slid = total_slide * (1 - ease_in_cubic(1 - (time_left / EXIT_MS)))
  else
    distance_slid = total_slide
  end
  distance_slid = math.floor(distance_slid + 0.5)

  local visible_w = math.min(distance_slid, pill_w_at_rest)
  if visible_w <= 0 then return nil end

  local top_rule    = "╭" .. string.rep("─", pill_w_at_rest - 1)
  local mid_text    = "│ " .. text .. string.rep(" ", RIGHT_OVERFLOW)
  local bottom_rule = "╰" .. string.rep("─", pill_w_at_rest - 1)

  local body = tui.column {
    gap = 0,
    key = "toast-box",
    children = {
      tui.constrained {
        max_height = 1,
        child = tui.text { content = top_rule, style = pal.border, wrap = "none" },
      },
      tui.constrained {
        max_height = 1,
        child = tui.text { content = mid_text, style = { fg = pal.fg }, wrap = "none" },
      },
      tui.constrained {
        max_height = 1,
        child = tui.text { content = bottom_rule, style = pal.border, wrap = "none" },
      },
    },
  }

  return tui.anchored {
    anchor   = "bottom-right",
    offset_x = 0,
    offset_y = 0,
    width    = visible_w,
    height   = FULL_HEIGHT,
    child    = body,
  }
end

return M
