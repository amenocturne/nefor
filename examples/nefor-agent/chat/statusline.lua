-- Bottom-row statusline + the welcome banner that pre-loads the empty
-- transcript. Both are pure view functions — they read state and
-- return tui nodes.

local common = require("libs.chat.common")
local C       = common.C
local STYLE   = common.STYLE
local context_bar = require("chat.context_bar")
local usage_view = require("libs.chat.usage")
local quota_policy = require("libs.chat.quota_policy")
local extensions = require("libs.chat.extensions")
local ok_config, config = pcall(require, "config")
local usage_config = ok_config and config.active and config.active.usage or {
  statusline_ids = { "chatgpt/subscription", "openrouter/session-total" },
}

local M = {}

M.MODE_BORDER_STYLES = {
  default = STYLE.input_border,
}

function M.input_border_style(state, focused)
  local canonical = not focused and STYLE.input_border_unfocused
    or M.MODE_BORDER_STYLES[state.mode or "default"] or STYLE.input_border
  return extensions.input_border_style(state, focused, canonical)
end

local function build_segments(state)
  local segs = extensions.status_segments(state)
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

  local function add_usage(usage_id)
    local value = type(state.usage) == "table" and state.usage[usage_id] or nil
    local usage_text, available = usage_view.footer(value)
    if usage_text == nil then return end
    local fg = C.system
    if available ~= nil and available <= 10 then fg = C.status_danger
    elseif available ~= nil and available <= 25 then fg = C.status_warn end
    segs[#segs + 1] = { spans = { { text = usage_text, fg = fg } } }
  end
  if quota_policy.surfaces_enabled(usage_config) then
    for _, usage_id in ipairs(usage_config.statusline_ids or {}) do add_usage(usage_id) end
  end

  return segs, context_bar.segment(
    state.current_context_tokens, state.max_tokens, state.max_context_tokens)
end

function M.view(state, pane_width)
  local segs, context = build_segments(state)
  local identity = {}
  for i, seg in ipairs(segs) do
    if i > 1 then identity[#identity + 1] = { text = " │ ", fg = C.status_dim } end
    for _, sp in ipairs(seg.spans) do identity[#identity + 1] = sp end
  end
  local used = context.used.text
  -- Reserve the count/dot/percentage before yielding bar cells. Constraining
  -- (not expanding) the preceding widgets keeps a short footer inline, while
  -- long identities yield before the measurement prefix, and the bar uses
  -- only the remaining cells (at most its normal 30).
  local meter = tui.constrained { max_width = 45, child = tui.row { gap = 0, children = {
    tui.spans { spans = {
      { text = " │ ", fg = C.status_dim },
      { text = string.rep(" ", math.max(0, 4 - #used)) .. used
        .. " · " .. string.format("%-4s", context.percent .. "%") .. " ", fg = C.system },
    }, wrap = "none" },
    tui.expanded { child = tui.spans { spans = context.bar, wrap = "none" } },
  } } }
  return tui.row { gap = 0, children = {
    tui.constrained { max_width = pane_width and math.max(0, pane_width - 15) or nil,
      child = tui.spans { spans = identity, wrap = "none" } },
    meter,
  } }
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
