-- Popup views. Five variants share the same shell — bordered box,
-- title row, scrollable body — wrapped by W.popup.view. Variant
-- discriminator lives on state.popup; render returns nil when no
-- popup is active or the variant doesn't match.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget
local common  = require("chat.common")
local sessions = require("chat.sessions")

local STYLE         = common.STYLE
local C             = common.C
local MARKDOWN_THEME = common.MARKDOWN_THEME
local CURSOR_ROW_STYLE = common.CURSOR_ROW_STYLE
local compact       = common.compact

local M = {}

local HELP_BODY = [[Keys:
  Enter        send message
  Shift+Enter  insert newline
  Esc          cancel current turn
  Esc Esc      cancel everything (within 600ms)
  Ctrl+B       toggle sidebar
  Ctrl+O       expand/collapse tool calls + reasoning
  ?            this help (when input empty)
  Up / Down    scroll transcript by one line
  PgUp / PgDn  scroll transcript by one page
  Home / End   jump to top / bottom
  Ctrl+C       quit
  Ctrl+D       quit

Slash commands:
  /new /clear  new chat (clears transcript)
  /help        this help
  /quit /exit  exit nefor
  /safe /auto /yolo  set tool permission mode
  /login /logout  provider auth
  /model       list/switch model
  /resume      resume a previous session]]

function M.help(state)
  if not state.popup or state.popup.variant ~= "help" then return nil end
  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_user,
    width        = "60%",
    height       = "60%",
    scroll_key   = "popup_help",
    title        = "── help ──",
    title_style  = STYLE.popup_user,
    child        = tui.column { gap = 1, children = {
      tui.text { content = HELP_BODY, wrap = "word" },
      tui.text { content = "(Esc / Q / Enter to close)", style = STYLE.status_dim },
    }},
  })
end

function M.message(state)
  if not state.popup then return nil end
  local v = state.popup.variant
  if v ~= "info" and v ~= "warning" and v ~= "error" then return nil end
  local title_style, glyph, border_style
  if v == "info" then
    title_style, glyph, border_style = STYLE.popup_info, "ℹ", STYLE.popup_info
  elseif v == "warning" then
    title_style, glyph, border_style = STYLE.popup_warn, "⚠", STYLE.popup_warn
  else
    title_style, glyph, border_style = STYLE.popup_danger, "✕", STYLE.popup_danger
  end
  local title = string.format(" %s %s %s ", v, glyph, state.popup.title or "")
  return W.popup.view({
    open         = true,
    border_style = border_style,
    width        = "60%",
    height       = "50%",
    scroll_key   = "popup_message",
    title        = title,
    title_style  = title_style,
    child        = tui.column { gap = 1, children = compact {
      tui.markdown { source = state.popup.body or "", theme = MARKDOWN_THEME, wrap = "word" },
      state.popup.source and tui.text {
        content = "from: " .. state.popup.source,
        style   = STYLE.footer,
      } or nil,
      tui.text { content = "Esc / Q to close", style = STYLE.status_dim },
    }},
  })
end

function M.tool_permission(state)
  if not state.popup or state.popup.variant ~= "tool_permission" then return nil end
  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_warn,
    width        = "60%",
    height       = "50%",
    scroll_key   = "popup_tool_permission",
    title        = " permission requested · " .. (state.popup.tool or "?"),
    title_style  = STYLE.popup_warn,
    child        = tui.column { gap = 1, children = {
      tui.text { content = state.popup.body or "", wrap = "word" },
      tui.text {
        content = "[A]pprove   [D]eny   (Esc = deny)",
        style   = STYLE.status_warn,
      },
    }},
  })
end

-- Filter the model picker's list against the typed query. Substring
-- match (case-insensitive) against "<provider> <model>". Stable sort
-- order preserved by walking the source list in order.
function M.model_picker_filter(models, query)
  if models == nil then return {} end
  local q = (query or ""):lower()
  if q == "" then return models end
  local out = {}
  for _, e in ipairs(models) do
    local s = ((e.provider or "") .. " " .. (e.model or "")):lower()
    if s:find(q, 1, true) ~= nil then out[#out + 1] = e end
  end
  return out
end

local function awaiting_count(awaiting)
  if awaiting == nil then return 0 end
  local n = 0
  for _, _ in pairs(awaiting) do n = n + 1 end
  return n
end

function M.session_picker(state)
  if not state.popup or state.popup.variant ~= "session_picker" then return nil end
  local p = state.popup
  local rows = p.sessions or {}
  local empty_child = tui.column { gap = 0, children = {
    tui.text {
      content = "No saved sessions found.",
      style   = STYLE.status_dim, wrap = "word",
    },
    tui.text {
      content = "Sessions live at " .. (sessions.session_dir() or "<unknown>"),
      style   = STYLE.status_dim, wrap = "word",
    },
  }}
  local picker_body
  if #rows == 0 then
    picker_body = empty_child
  else
    picker_body = W.picker.view({
      state        = { cursor = p.cursor or 1 },
      entries      = function() return rows end,
      format_entry = function(s)
        local stamp = sessions.format_started_at(s.started_at)
        local preview = sessions.clip_preview(s.preview, 50)
        return string.format("%-12s  %s", stamp, preview)
      end,
      cursor_style = CURSOR_ROW_STYLE,
      row_style    = STYLE.status,
      show_search  = false,
      cap          = 12,
    })
  end
  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_user,
    width        = "70%",
    height       = "60%",
    scroll_key   = "popup_session_picker",
    title        = "── resume a session ──",
    title_style  = STYLE.popup_user,
    child        = tui.column { gap = 1, children = {
      picker_body,
      tui.text {
        content = "↑/↓ select · Enter resume · Esc cancel",
        style   = STYLE.status_dim,
        wrap    = "none",
      },
    }},
  })
end

-- Model picker rendered as per-provider sections. Each provider gets:
--
--   <provider>  [connected]      ← green header for connected
--     model-a
--     model-b
--
--   <provider>  [disconnected]   ← red header for login_required/error
--     (log in to load models)
--
-- Cursor navigates over the flat list of selectable model rows;
-- section headers and placeholder text are visual-only and skipped.
-- The text tag complements the colour so the picker stays readable in
-- terminals without colour.
function M.model_picker(state)
  if not state.popup or state.popup.variant ~= "model_picker" then return nil end
  local p = state.popup
  local providers = p.providers or {}
  local query_lc = (p.query or ""):lower()

  local function matches_query(model_name)
    if query_lc == "" then return true end
    return tostring(model_name):lower():find(query_lc, 1, true) ~= nil
  end

  -- First pass: build the flat list of selectable rows so we can
  -- clamp the cursor and map cursor→provider/model on Enter.
  local flat_rows = {}
  local section_models = {}  -- per-provider filtered model lists, parallel to providers[]
  for pi, prov in ipairs(providers) do
    local filtered = {}
    for _, m in ipairs(prov.models or {}) do
      if matches_query(m) then
        filtered[#filtered + 1] = m
        flat_rows[#flat_rows + 1] = { provider = prov.name, model = m }
      end
    end
    section_models[pi] = filtered
  end

  local cursor = p.cursor or 1
  if cursor < 1 then cursor = 1 end
  if #flat_rows > 0 and cursor > #flat_rows then cursor = #flat_rows end

  local function state_tag(s)
    if s == "connected"      then return "[connected]"    end
    if s == "login_required" then return "[disconnected]" end
    if s == "error"          then return "[disconnected]" end
    return "[" .. tostring(s or "unknown") .. "]"
  end

  local function header_style(s)
    if s == "connected" then return STYLE.status_ok end
    return STYLE.status_danger
  end

  local children = {}
  children[#children + 1] = tui.text {
    content = "search: " .. (p.query or ""),
    style   = STYLE.status, wrap = "none",
  }
  children[#children + 1] = tui.text {
    content = string.rep("─", 40),
    style   = STYLE.footer, wrap = "none",
  }

  if #providers == 0 then
    children[#children + 1] = tui.text {
      content = "No providers registered.",
      style   = STYLE.status_dim, wrap = "word",
    }
  end

  -- Tracks the cursor's position in flat_rows so we can highlight the
  -- right model row during the per-section render pass.
  local flat_idx = 0

  for pi, prov in ipairs(providers) do
    if pi > 1 then
      children[#children + 1] = tui.text {
        content = "", style = STYLE.status, wrap = "none",
      }
    end
    children[#children + 1] = tui.text {
      content = prov.name .. "  " .. state_tag(prov.state),
      style   = header_style(prov.state),
      wrap    = "none",
    }
    local filtered = section_models[pi]
    if #filtered == 0 then
      local hint
      if prov.state == "connected" then
        if p.awaiting and p.awaiting[prov.name] then
          hint = "  (loading…)"
        elseif query_lc ~= "" then
          hint = "  (no matches)"
        else
          hint = "  (no models)"
        end
      else
        hint = "  (log in to load models)"
      end
      children[#children + 1] = tui.text {
        content = hint, style = STYLE.status_dim, wrap = "none",
      }
    else
      for _, m in ipairs(filtered) do
        flat_idx = flat_idx + 1
        local style = (flat_idx == cursor) and CURSOR_ROW_STYLE or STYLE.status
        children[#children + 1] = tui.text {
          content = "  " .. m, style = style, wrap = "none",
        }
      end
    end
  end

  children[#children + 1] = tui.text {
    content = "", style = STYLE.status, wrap = "none",
  }
  children[#children + 1] = tui.text {
    content = "↑/↓ select · Enter pick · Esc close · type to filter",
    style   = STYLE.status_dim, wrap = "none",
  }

  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_user,
    width        = "60%",
    height       = "60%",
    scroll_key   = "popup_model_picker",
    title        = "── pick a model ──",
    title_style  = STYLE.popup_user,
    child        = tui.column { gap = 0, children = children },
  })
end

-- Provider login picker. Lists every provider the engine knows about
-- with its current auth state as a right-aligned tag so the user can
-- pick which one to authenticate. Selection emits
-- `chat.login_requested` or `chat.logout_requested` depending on
-- `popup.mode` ("login" or "logout"; defaults to "login").
function M.login_picker(state)
  if not state.popup or state.popup.variant ~= "login_picker" then return nil end
  local p = state.popup
  local mode = p.mode or "login"
  local rows = p.providers or {}
  local prov_w = 0
  for _, r in ipairs(rows) do
    if #r.name > prov_w then prov_w = #r.name end
  end
  if prov_w > 20 then prov_w = 20 end

  local function state_tag(s)
    if s == "connected"      then return "[connected]" end
    if s == "login_required" then return "[log in]"    end
    if s == "error"          then return "[error]"     end
    return "[" .. tostring(s or "unknown") .. "]"
  end

  local empty_text
  if #rows == 0 then
    if mode == "logout" then
      empty_text = "No connected providers to log out from."
    else
      empty_text = "No providers registered yet."
    end
  end

  local title  = (mode == "logout") and "── log out from provider ──" or "── log in to provider ──"
  local footer = (mode == "logout")
    and "↑/↓ select · Enter logout · Esc cancel"
    or  "↑/↓ select · Enter login · Esc cancel"

  local picker_body = W.picker.view({
    state          = { cursor = p.cursor or 1 },
    entries        = function() return rows end,
    format_entry   = function(r)
      return string.format("%-" .. prov_w .. "s  %s", r.name, state_tag(r.state))
    end,
    cursor_style   = CURSOR_ROW_STYLE,
    row_style      = STYLE.status,
    show_search    = false,
    empty_style    = STYLE.status_dim,
    empty_text     = empty_text,
    cap            = 12,
    gap            = 0,
  })

  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_user,
    width        = "50%",
    height       = "40%",
    scroll_key   = "popup_login_picker",
    title        = title,
    title_style  = STYLE.popup_user,
    child        = tui.column { gap = 1, children = {
      picker_body,
      tui.text {
        content = footer,
        style   = STYLE.status_dim, wrap = "none",
      },
    }},
  })
end

-- Map popup variant → inner scrollable key. Used by the scroll-key
-- router in update.lua so PgUp/PgDn route to the active popup's
-- scrollable rather than the transcript.
function M.scroll_key(variant)
  if variant == "help"            then return "popup_help" end
  if variant == "info"
     or variant == "warning"
     or variant == "error"        then return "popup_message" end
  if variant == "tool_permission" then return "popup_tool_permission" end
  if variant == "model_picker"    then return "popup_model_picker" end
  if variant == "session_picker"  then return "popup_session_picker" end
  if variant == "login_picker"    then return "popup_login_picker" end
  return nil
end

return M
