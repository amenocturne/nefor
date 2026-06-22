-- Bordered text-input widget with completion popup and history nav.
--
-- State slice held by the caller's reducer:
--   { value = "...",
--     completion = nil | { trigger, token, matches, cursor, ... },
--     history_cursor = nil | integer }
--
-- Opts:
--   state                 widget's state slice (required)
--   key                   text_input key (default "prompt")
--   focused               whether the input takes keystrokes
--   border_style          chrome style when focused
--   unfocused_style       chrome style when not focused
--   border_key            outer column key (default "prompt-field")
--   min_lines, max_lines  text_input geometry
--   on_change, on_submit  message kinds dispatched to the reducer
--   completions           list of { trigger, anchor, source, filter,
--                          format_entry, apply } entries
--   history               list (or zero-arg fn returning list) of prior
--                          submitted strings; newest at index 1

local util = require("nefor-tui.util")

local M = {}

-- Find the active token in `text` for the given trigger/anchor. Returns
-- (token_with_trigger, token_start_pos, token_body) or nils.
function M.active_token(text, trigger, anchor)
  if text == nil or text == "" then return nil end
  if anchor == "start" then
    if text:sub(1, 1) ~= trigger then return nil end
    if text:find("%s") ~= nil then return nil end
    return text, 1, text:sub(2)
  end
  if anchor == "start-spaced" then
    if text:sub(1, 1) ~= trigger then return nil end
    return text, 1, text:sub(2)
  end
  -- anchor "word": last `trigger` at position 1 or after whitespace,
  -- with no whitespace in the body that follows.
  local pos = nil
  for i = #text, 1, -1 do
    local c = text:sub(i, i)
    if c == trigger then
      if i == 1 or text:sub(i - 1, i - 1):match("%s") then
        pos = i
      end
      break
    end
    if c:match("%s") then break end
  end
  if pos == nil then return nil end
  local body = text:sub(pos + 1)
  if body:find("%s") ~= nil then return nil end
  return text:sub(pos), pos, body
end

local function match_completion(opts, text)
  if opts.completions == nil then return nil end
  for _, c in ipairs(opts.completions) do
    if c.trigger ~= nil then
      local token, pos, body = M.active_token(text, c.trigger, c.anchor or "word")
      if token ~= nil then
        return c, token, pos, body
      end
    end
  end
  return nil
end

-- Compute the completion-state patch for `text`. Returns a sub-patch
-- table (possibly `{ completion = util.NIL }`); never mutates.
local function compute_completion(opts, text, prev)
  local cfg, token, _pos, body = match_completion(opts, text)
  if cfg == nil then
    return { completion = util.NIL }
  end
  if prev and prev.token == token then
    return {}
  end
  -- `source(body)` lets the caller derive a per-keystroke listing from
  -- the trigger body — @-path autocomplete uses it to scope the
  -- directory listing to the prefix in body.
  local entries = cfg.source and cfg.source(body or "") or {}
  local matches
  if cfg.filter ~= nil then
    matches = cfg.filter(entries, body or "")
  else
    local q = (body or ""):lower()
    matches = {}
    for _, e in ipairs(entries) do
      local s = (type(e) == "table" and (e.name or e.label or tostring(e)) or tostring(e)):lower()
      if s:sub(1, #q) == q then matches[#matches + 1] = e end
    end
  end
  return {
    completion = {
      trigger      = cfg.trigger,
      anchor       = cfg.anchor or "word",
      token        = token,
      body         = body,
      matches      = matches,
      cursor       = 1,
      format_entry = cfg.format_entry,
      apply        = cfg.apply,
    },
  }
end

local function default_apply(entry, body, value)
  local replacement = "/" .. (type(entry) == "table" and (entry.name or tostring(entry)) or tostring(entry))
  local _ = body
  local _ = value
  return replacement
end

-- Inline autocomplete dropdown rendered above the input. Returns nil
-- when no completion is active.
local function autocomplete_view(state, completions)
  local c = state and state.completion
  if c == nil then return nil end
  local matches = c.matches or {}
  if #matches == 0 then
    return tui.text {
      content = "no matches",
      style   = (completions and completions.empty_style) or nil,
      wrap    = "none",
    }
  end
  local format = c.format_entry or function(e)
    if type(e) == "table" then return e.name or tostring(e) end
    return tostring(e)
  end
  local cap = 8
  local cursor = c.cursor or 1
  local first = math.max(1, math.min(cursor - cap + 1, #matches - cap + 1))
  if first < 1 then first = 1 end
  local last = math.min(first + cap - 1, #matches)
  local children = {}
  for i = first, last do
    local display = format(matches[i])
    children[#children + 1] = tui.text {
      content = display,
      style   = (i == cursor) and (completions and completions.cursor_style) or nil,
      wrap    = "none",
    }
  end
  return tui.column { gap = 0, children = children }
end

function M.view(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("prompt.view: opts must be a table, got " .. type(opts))
  end
  local state = opts.state or {}
  if type(state) ~= "table" then
    error("prompt.view: opts.state must be a table, got " .. type(state))
  end
  local focused = opts.focused ~= false
  local border_style = focused
    and opts.border_style
    or  (opts.unfocused_style or opts.border_style)

  local input = tui.text_input {
    key        = opts.key or "prompt",
    value      = state.value or "",
    focused    = focused,
    on_change  = opts.on_change or "prompt.changed",
    on_submit  = opts.on_submit or "prompt.submit",
    min_lines  = opts.min_lines or 1,
    max_lines  = opts.max_lines or 6,
    selectable = opts.selectable ~= false,
  }

  local field = util.bordered_box(input, border_style,
    opts.border_key or "prompt-field")

  local autocomplete = autocomplete_view(state, opts.completions_view)
  if autocomplete == nil then
    return field
  end
  return tui.column {
    gap = 0,
    children = { autocomplete, field },
  }
end

-- Exposed for callers that want to embed the autocomplete row themselves
-- (e.g. above an input nested deeper in their layout column).
function M.autocomplete_view(state, completions)
  return autocomplete_view(state, completions)
end

-- Apply the cursor-selected completion entry. Returns the new input
-- value or nil when there's nothing to apply.
function M.apply_completion(opts, state)
  local c = state and state.completion
  if c == nil then return nil end
  local entry = c.matches and c.matches[c.cursor or 1]
  if entry == nil then return nil end
  local value = state.value or ""
  if c.apply ~= nil then
    return c.apply(entry, c.body or "", value, c.token or "")
  end
  if c.anchor == "start" then
    return default_apply(entry, c.body or "", value)
  end
  local _, pos = M.active_token(value, c.trigger, c.anchor or "word")
  if pos == nil then return nil end
  local name = type(entry) == "table" and (entry.name or tostring(entry)) or tostring(entry)
  return value:sub(1, pos - 1) .. c.trigger .. name
end

-- Re-compute the completion-state patch from current value. Lets the
-- caller re-open the dropdown after externally editing `value` (e.g.
-- after applying a directory-style entry).
function M.refresh_completion(opts, state)
  return compute_completion(opts, state.value or "", state.completion)
end

-- Handle a reducer message. Returns `{ state = <patch> }` when consumed,
-- nil otherwise. Patch keys equal to util.NIL clear the slot.
function M.handle(opts, msg)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("prompt.handle: opts must be a table, got " .. type(opts))
  end
  if msg == nil or msg.kind == nil then return nil end
  local state = opts.state or {}
  local kind = msg.kind

  local on_change = opts.on_change or "prompt.changed"
  if kind == on_change then
    local v = msg.value or ""
    local patch = { value = v, history_cursor = util.NIL }
    local comp = compute_completion(opts, v, state.completion)
    for k, val in pairs(comp) do patch[k] = val end
    return { state = patch }
  end

  if state.completion ~= nil then
    local c = state.completion
    if kind == "key.up" then
      local n = #(c.matches or {})
      if n == 0 then return { state = {} } end
      local cur = (c.cursor or 1) - 1
      if cur < 1 then cur = n end
      return { state = { completion = util.shallow_merge(c, { cursor = cur }) } }
    end
    if kind == "key.down" then
      local n = #(c.matches or {})
      if n == 0 then return { state = {} } end
      local cur = (c.cursor or 1) + 1
      if cur > n then cur = 1 end
      return { state = { completion = util.shallow_merge(c, { cursor = cur }) } }
    end
    if kind == "key.tab" then
      local nv = M.apply_completion(opts, state)
      if nv == nil then return { state = {} } end
      local patch = { value = nv, completion = util.NIL }
      local refresh = compute_completion(opts, nv, nil)
      for k, val in pairs(refresh) do patch[k] = val end
      return { state = patch }
    end
    if kind == "key.escape" then
      return { state = { completion = util.NIL } }
    end
  end

  local history = opts.history
  if type(history) == "function" then history = history() end
  history = history or {}
  if kind == "key.up" and (state.completion == nil) then
    local navigating = state.history_cursor ~= nil
    local empty = (state.value or "") == ""
    if (navigating or empty) and #history > 0 then
      local cur = state.history_cursor or 0
      local nxt = math.min(cur + 1, #history)
      return {
        state = {
          value = history[nxt],
          history_cursor = nxt,
        },
      }
    end
  end
  if kind == "key.down" and (state.completion == nil) then
    if state.history_cursor ~= nil then
      local cur = state.history_cursor
      if cur <= 1 then
        return { state = { value = "", history_cursor = util.NIL } }
      end
      local nxt = cur - 1
      return {
        state = {
          value = history[nxt],
          history_cursor = nxt,
        },
      }
    end
  end

  return nil
end

return M
