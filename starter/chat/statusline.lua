-- Bottom-row statusline + the welcome banner that pre-loads the empty
-- transcript. Both are pure view functions — they read state and
-- return tui nodes.

local common = require("libs.chat.common")
local C       = common.C
local STYLE   = common.STYLE
local humanize_tokens = common.humanize_tokens
local usage_view = require("libs.chat.usage")

local M = {}

M.MODE_BORDER_STYLES = {
  default = STYLE.input_border,
}

function M.input_border_style(state, focused)
  if not focused then return STYLE.input_border_unfocused end
  return M.MODE_BORDER_STYLES[state.mode or "default"] or STYLE.input_border
end

local function ctx_bar(used, max)
  if used == nil and max ~= nil and max ~= 0 then
    return {
      spans = {
        { text = "ctx " .. (humanize_tokens(max) or tostring(max)), fg = C.system },
      },
    }
  end
  if used == nil then return nil end
  if max == nil or max == 0 then
    return {
      spans = {
        { text = "ctx " .. (humanize_tokens(used) or tostring(used)), fg = C.system },
      },
    }
  end
  local pct = math.floor(100 * used / max + 0.5)
  if pct < 0 then pct = 0 end
  if pct > 100 then pct = 100 end
  local bar_w = 8
  local filled = math.floor(bar_w * used / max + 0.5)
  if filled < 0 then filled = 0 end
  if filled > bar_w then filled = bar_w end
  local empty = bar_w - filled
  local bar_style
  if pct >= 90 then
    bar_style = STYLE.status_danger
  elseif pct >= 70 then
    bar_style = STYLE.status_warn
  else
    bar_style = STYLE.status_bar_fill
  end
  local label = string.format("ctx %s/%s ",
    humanize_tokens(used) or tostring(used),
    humanize_tokens(max)  or tostring(max))
  return {
    spans = {
      { text = label, fg = C.system },
      { text = string.rep("█", filled), fg = bar_style.fg },
      { text = string.rep("░", empty),  fg = C.status_dim },
      { text = " " .. tostring(pct) .. "%", fg = C.system },
    },
  }
end

local function build_segments(state)
  local segs = {}
  if state.gate_mode == "safe" then
    segs[#segs + 1] = { spans = { { text = "SAFE", fg = C.status_ok, bold = true } } }
  elseif state.gate_mode == "yolo" then
    segs[#segs + 1] = { spans = { { text = "YOLO", fg = C.status_danger, bold = true } } }
  elseif state.gate_mode == "auto" then
    segs[#segs + 1] = { spans = { { text = "AUTO", fg = C.status_warn, bold = true } } }
  end
  local model = state.model or (state.stats and state.stats.model)
  if model then
    local label = model
    if type(state.reasoning_effort) == "string" and #state.reasoning_effort > 0 then
      label = label .. " · " .. state.reasoning_effort
    end
    segs[#segs + 1] = { spans = { { text = label, fg = C.system } } }
  else
    segs[#segs + 1] = { spans = { { text = "Start chatting to see stats", fg = C.status_dim } } }
  end

  local s = state.stats or {}
  local last_ctx = s.last_turn_context_tokens or s.last_turn_input_tokens or s.context_tokens or s.prompt_tokens
  if last_ctx or state.max_tokens then
    local cb = ctx_bar(last_ctx, state.max_tokens)
    if cb then segs[#segs + 1] = cb end
  end

  if s.cost_usd ~= nil then
    segs[#segs + 1] = { spans = { { text = string.format("$%.2f", s.cost_usd), fg = C.system } } }
  end
  local active_usage = type(state.usage) == "table" and state.usage[state.provider] or nil
  local usage_text, available = usage_view.footer(active_usage)
  if usage_text ~= nil then
    local fg = C.system
    if available <= 10 then fg = C.status_danger
    elseif available <= 25 then fg = C.status_warn end
    segs[#segs + 1] = { spans = { { text = usage_text, fg = fg } } }
  end

  return segs
end

function M.view(state)
  local segs = build_segments(state)
  local out_spans = {}
  for i, seg in ipairs(segs) do
    if i > 1 then
      out_spans[#out_spans + 1] = { text = " │ ", fg = C.status_dim }
    end
    for _, sp in ipairs(seg.spans) do
      out_spans[#out_spans + 1] = sp
    end
  end
  return tui.spans { spans = out_spans }
end

-- Welcome banner.
-- Painted on a truly fresh chat surface (no entries, no in-flight
-- turn). Disappears the instant the user submits.
--
-- To disable: set WELCOME_BANNER_LINES to {} (empty list). To
-- customize: edit the strings; each entry is one centered line. Style
-- is shared via STYLE.status_dim.

local WELCOME_BANNER_LINES = {
  "Welcome to the starter config!",
  "",
  "This is your average agentic workflow, but nefor can do much more than that.",
  "",
  "Experiment and do whatever you want!",
}

function M.welcome_banner()
  if #WELCOME_BANNER_LINES == 0 then return nil end
  local rows = {}
  for i, line in ipairs(WELCOME_BANNER_LINES) do
    -- Each line: a 1-row-tall tui.align{center} so the line centers
    -- horizontally within the chat column. The height clamp is
    -- load-bearing — without it tui.align greedily takes the
    -- available height (it fills its slot at parent max), which
    -- would collapse subsequent rows to zero height. With
    -- max_height=1 each align resolves to exactly one row.
    rows[i] = tui.constrained {
      max_height = 1,
      child = tui.align {
        alignment = "center",
        child = tui.text {
          content = line,
          style   = STYLE.status_dim,
          wrap    = "none",
        },
      },
    }
  end
  return tui.align {
    alignment = "center",
    child = tui.column { gap = 0, children = rows },
  }
end

return M
