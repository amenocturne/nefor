-- starter/chat.lua — chat surface as a Lua composition over tui.* primitives.
--
-- Visual + behavioral parity with the legacy `nefor-chat` plugin. All hex
-- codes, glyphs, segment ordering, and keymap are sourced from the
-- reverse-engineered spec at
--
-- Architecture (per nefor-tui-declarative-spec): the engine ships zero
-- opinion. Every color, every layout, every glyph below is editable —
-- this file IS the chat surface's identity.
--
-- Inbound chat-contract events handled here:
--   chat.message.append, chat.stream.delta, chat.stream.end,
--   chat.stream.reasoning_delta, chat.stream.reasoning_end,
--   chat.session.stats, chat.tool.start, chat.tool.end,
--   chat.popup, chat.auth.status, chat.model.set_ack,
--   tool-gate.permission_request,
--   graph.run_started, graph.node_dispatched, graph.node_result,
--   graph.run_complete.
--
-- Outbound:
--   chat.input.submit, chat.interrupt, chat.interrupt_all, chat.reset,
--   chat.command, tool.permission_response.

------------------------------------------------------------------------
-- helpers
------------------------------------------------------------------------

local function map(list, fn)
  local out = {}
  for i, v in ipairs(list) do out[i] = fn(v, i) end
  return out
end

-- Sentinel for `nil`-as-set-value in shallow_merge — Lua's `pairs`
-- doesn't yield keys mapped to nil, so passing `{ x = nil }` to a merge
-- can't distinguish "unset x" from "leave x alone". Wrap a nil as
-- `NIL_SENTINEL` to force the merge to clear the key.
local NIL_SENTINEL = {}

local function shallow_merge(a, b)
  local out = {}
  for k, v in pairs(a) do out[k] = v end
  for k, v in pairs(b) do
    if v == NIL_SENTINEL then
      out[k] = nil
    else
      out[k] = v
    end
  end
  return out
end

-- Compact a list that may contain nils. We can't use `ipairs` because it
-- stops at the first nil (a fundamental quirk of Lua's array semantics:
-- `{ a, nil, c }` has length 1 from ipairs's perspective). Instead, walk
-- a numeric range up to the table's "border" approximated by `#list`,
-- but inspect every slot manually so trailing entries past a nil hole
-- are visited.
local function compact(list)
  local out = {}
  -- Find the highest-set numeric key so we cover holes safely.
  local maxn = 0
  for k, _ in pairs(list) do
    if type(k) == "number" and k > maxn then maxn = k end
  end
  for i = 1, maxn do
    local v = list[i]
    if v ~= nil then out[#out + 1] = v end
  end
  return out
end

local function strip_claude_prefix(model)
  if model == nil then return nil end
  local stripped = model:gsub("^claude%-", "")
  return stripped
end

local function humanize_duration_ms(ms)
  if ms == nil then return nil end
  if ms < 1000 then return tostring(math.floor(ms)) .. "ms" end
  if ms < 60000 then return string.format("%ds", math.floor(ms / 1000)) end
  local m = math.floor(ms / 60000)
  local s = math.floor((ms % 60000) / 1000)
  return string.format("%dm%02ds", m, s)
end

local function humanize_tokens(n)
  if n == nil then return nil end
  if n < 1000 then return tostring(n) end
  if n < 1000000 then return tostring(math.floor(n / 1000)) .. "k" end
  return string.format("%.1fM", n / 1000000)
end

------------------------------------------------------------------------
-- styling — exact legacy hex codes
------------------------------------------------------------------------

-- Palette per legacy spec section 2.
local C = {
  user            = "#7FB4FF",  -- HL_USER, HL_STATUS_BAR_FILL, HL_MD_LIST_MARKER, HL_MD_LINK, HL_STATUS_INFO
  system          = "#808080",  -- HL_SYSTEM, HL_STATUS, HL_REASONING, HL_MD_QUOTE_BAR
  status_dim      = "#606060",  -- HL_STATUS_DIM
  status_warn     = "#D7AF5F",  -- HL_STATUS_WARN
  status_danger   = "#D75F5F",  -- HL_STATUS_DANGER
  status_ok       = "#87D787",  -- HL_STATUS_OK
  md_heading      = "#FFB86C",  -- HL_MD_HEADING (Dracula orange)
  md_code_fg      = "#C0C0C0",  -- HL_MD_CODE_INLINE / HL_MD_CODE_BLOCK fg
  md_code_inline_bg = "#303030",
  md_code_block_bg  = "#202020",
  footer          = "#707070",  -- HL_FOOTER
}

local STYLE = {
  user_chrome     = { fg = C.user, bold = true },         -- ╭─╰│ borders
  -- Input-field border. Same blue as user blocks per legacy spec
  -- section 1 (input top/bot bars in HL_USER).
  input_border          = { fg = C.user, bold = true },
  input_border_unfocused= { fg = C.status_dim },          -- dim when no focus
  body_default    = nil,                                  -- HL_ASSISTANT = terminal default
  system          = { fg = C.system, italic = true },
  status          = { fg = C.system },
  status_dim      = { fg = C.status_dim },
  status_warn     = { fg = C.status_warn },
  status_danger   = { fg = C.status_danger, bold = true },
  status_ok       = { fg = C.status_ok },
  status_info     = { fg = C.user },
  status_bar_fill = { fg = C.user },
  footer          = { fg = C.footer },
  reasoning       = { fg = C.system, italic = true },
  tool_name       = { fg = C.md_heading, bold = true },
  tool_error      = { fg = C.status_danger, bold = true },
  popup_user      = { fg = C.user, bold = true },
  popup_warn      = { fg = C.status_warn, bold = true },
  popup_danger    = { fg = C.status_danger, bold = true },
  popup_info      = { fg = C.user, bold = true },
  toast           = { fg = C.user },
  -- DAG panel
  dag_separator   = { fg = C.footer },
  dag_pending     = { fg = C.status_dim },
  dag_running     = { fg = C.status_warn },
  dag_done        = { fg = C.status_ok },
  dag_error       = { fg = C.status_danger, bold = true },
  dag_skipped     = { fg = C.status_dim, italic = true },
}

-- Markdown theme — exact legacy hex codes (spec section 3).
local MARKDOWN_THEME = {
  bold        = { bold = true },
  italic      = { italic = true },
  code        = { fg = C.md_code_fg, bg = C.md_code_inline_bg },
  code_block  = { fg = C.md_code_fg, bg = C.md_code_block_bg },
  h1          = { fg = C.md_heading, bold = true },
  h2          = { fg = C.md_heading, bold = true },
  h3          = { fg = C.md_heading, bold = true },
  h4          = { fg = C.md_heading, bold = true },
  h5          = { fg = C.md_heading, bold = true },
  h6          = { fg = C.md_heading, bold = true },
  link        = { fg = C.user, underline = true },
  blockquote  = { fg = C.system, italic = true },
  list_marker = { fg = C.user },
}

------------------------------------------------------------------------
-- state
------------------------------------------------------------------------
--
--   entries           list. Per-entry shapes:
--                       { role = "user",      kind = "text", text }
--                       { role = "assistant", kind = "stream",
--                         text, model?, duration_ms?, streaming?, reasoning? }
--                       { role = "system",    kind = "text", text }
--                       { role = "tool",      kind = "tool_call",
--                         id, name, input, output?, error? }
--   in_flight         index of streaming assistant entry, or nil
--   input_value       text_input value
--   focused_id        focus key (only "input" for now)
--   show_sidebar      Ctrl+B toggle
--   popup             nil | { variant, title, body, source?, ... }
--                     variant ∈ "help" | "info" | "warning" | "error"
--                       | "model_picker" | "tool_permission" | "toast"
--   stats             chat.session.stats payload
--   pending           true after submit, false after first delta or stream end
--   turn_started_at   ms when the user submitted; cleared on stream end
--   last_turn_duration_ms  ms recorded when the most recent turn ended
--   model             active model
--   max_tokens        per-model context window (200k for opus/sonnet/haiku)
--   gate_yolo         whether the tool gate is in YOLO mode
--   auth              per-provider state map
--   expanded_details  Ctrl+O toggle: expand all tool I/O + reasoning rows
--   slash             nil | { matches, cursor, query }
--   last_esc_ms       ms of the most recent ESC press (for double-ESC)
--   dag_runs          map keyed by run_id

local DAG_LINGER_MS  = 2000
local DOUBLE_ESC_MS  = 600

local function initial_state()
  return {
    entries          = {},
    in_flight        = nil,
    input_value      = "",
    focused_id       = "input",
    show_sidebar     = false,
    popup            = nil,
    stats            = {},
    pending          = false,
    turn_started_at  = nil,
    last_turn_duration_ms = nil,
    model            = nil,
    max_tokens       = nil,
    gate_yolo        = false,
    auth             = {},
    expanded_details = false,
    slash            = nil,
    last_esc_ms      = nil,
    dag_runs         = {},
    toast            = nil,  -- { text, expires_at_ms }
  }
end

------------------------------------------------------------------------
-- slash registry
------------------------------------------------------------------------

local SLASH_COMMANDS = {
  { name = "new",     aliases = { "clear" }, hint = "start a fresh chat (clears transcript)", takes_args = false },
  { name = "help",    aliases = {},          hint = "show the help popup",                    takes_args = false },
  { name = "quit",    aliases = { "exit" },  hint = "exit nefor",                             takes_args = false },
  { name = "login",   aliases = {},          hint = "authenticate a provider",                takes_args = true },
  { name = "logout",  aliases = {},          hint = "revoke a provider's auth",               takes_args = true },
  { name = "model",   aliases = {},          hint = "list/switch active model",               takes_args = true },
  { name = "resume",  aliases = {},          hint = "resume previous session",                takes_args = true },
  { name = "yolo",    aliases = {},          hint = "disable tool permission prompts (DANGEROUS)", takes_args = false },
  { name = "safe",    aliases = {},          hint = "re-enable tool permission prompts",      takes_args = false },
  { name = "dag-test",aliases = {},          hint = "submit a 2-node parallel test DAG",      takes_args = false },
}

local function slash_filter(query)
  -- Case-insensitive prefix match against name OR aliases.
  local q = (query or ""):lower()
  local out = {}
  for _, cmd in ipairs(SLASH_COMMANDS) do
    local match = cmd.name:lower():sub(1, #q) == q
    if not match then
      for _, a in ipairs(cmd.aliases) do
        if a:lower():sub(1, #q) == q then match = true; break end
      end
    end
    if match then out[#out + 1] = cmd end
  end
  return out
end

------------------------------------------------------------------------
-- markdown rendering
------------------------------------------------------------------------

local function md(source)
  return tui.markdown { source = source or "", theme = MARKDOWN_THEME, wrap = "word" }
end

------------------------------------------------------------------------
-- bordered-box composition
--
-- Full-width rounded-corner box around an arbitrary child:
--   ╭──────────…──────╮
--   │ <child>         │
--   │ <child row 2>   │
--   ╰──────────…──────╯
--
-- Built from primitives — corners are `tui.text`, the rules are
-- `tui.expanded { child = tui.fill { char = "─" } }`, the side bars
-- are `tui.fill { char = "│" }` constrained to 1 column wide. The
-- side-bar fills inherit cross-axis stretch from the body row (CSS
-- `align-items: stretch` default in the `row`/`column` layout): if the
-- body is 4 rows tall, the side bars stretch to 4 rows automatically.
-- Without that stretch, a `tui.text { content = "│" }` side bar would
-- only paint row 0 and leave rows 1+ unbordered — the canonical
-- "popup with a missing border" bug.
--
-- Each rule row is still wrapped in `constrained { max_height = 1 }`
-- as defence-in-depth: the corner glyphs are 1-row tall so the row's
-- natural cross resolves to 1 anyway, but the explicit cap makes the
-- intent obvious in the source.
------------------------------------------------------------------------

local function rule_row(left_corner, right_corner, style)
  return tui.constrained {
    max_height = 1,
    child = tui.row {
      gap = 0,
      children = {
        tui.text { content = left_corner,  style = style, wrap = "none" },
        tui.expanded { child = tui.fill { char = "─", style = style } },
        tui.text { content = right_corner, style = style, wrap = "none" },
      },
    },
  }
end

-- Bordered box around `child`. `border_style` colors the corners,
-- rules, and side bars. Optional `key` stamps the outer column with a
-- stable user-key so the reconciler reuses the instance across renders
-- where the parent column's child positions shift (e.g. the input
-- field needs to keep text_input's per-instance cursor when the slash
-- autocomplete dropdown opens above it and pushes the input down a
-- slot — without a key, the column's `(type, position)` identity
-- changes and the whole subtree re-mounts, dropping cursor state).
local function bordered_box(child, border_style, key)
  local side_bar = tui.constrained {
    max_width = 1,
    child = tui.fill { char = "│", style = border_style },
  }
  local body_row = tui.row {
    gap = 0,
    children = {
      side_bar,
      -- Inset the body 1 col on each side so it doesn't touch the
      -- side bars. `tui.padding` reports `(child + h_pad, child)` so
      -- the body row's natural cross stays = the child's height; the
      -- side-bar fills then stretch to that height.
      tui.expanded {
        child = tui.padding { left = 1, right = 1, top = 0, bottom = 0, child = child },
      },
      side_bar,
    },
  }
  return tui.column {
    gap = 0,
    key = key,
    children = {
      rule_row("╭", "╮", border_style),
      body_row,
      rule_row("╰", "╯", border_style),
    },
  }
end

-- `bordered_box` variant for popups anchored to a fixed height. The
-- outer column distributes space top-down, non-flex first: when the
-- body's natural height ≥ the popup's allocated height it consumes the
-- full budget and the bottom rule is starved — the canonical "popup
-- with no bottom rule" bug.
--
-- Fix: make the body row flex (`tui.expanded`). Flex children skip
-- pass 1 and only get whatever's left after the two non-flex rule rows
-- claim their 1-row budget each, so the bottom `╰────╯` always paints.
-- Inside the flex body we wrap content in `tui.scrollable` so popup
-- bodies of any length render — the user can scroll if content
-- exceeds the visible area.
local function bordered_popup(scroll_key, child, border_style)
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
          left = 1, right = 1, top = 0, bottom = 0,
          child = tui.scrollable {
            key       = scroll_key,
            scrollbar = "auto",
            child     = child,
          },
        },
      },
      side_bar,
    },
  }
  return tui.column {
    gap = 0,
    children = {
      rule_row("╭", "╮", border_style),
      tui.expanded { child = body_row },
      rule_row("╰", "╯", border_style),
    },
  }
end

------------------------------------------------------------------------
-- entry rendering — per legacy spec section 5
------------------------------------------------------------------------

-- User entry: full-width bordered block in HL_USER. Body stays in
-- default fg per spec (`HL_ASSISTANT`-coloured text inside a HL_USER
-- frame).
local function render_user_entry(entry)
  return bordered_box(
    tui.text { content = entry.text or "", wrap = "word" },
    STYLE.user_chrome
  )
end

-- Reasoning rows above the assistant body. Per legacy spec section 5:
--   live  (streaming + body empty)  → "▼ thinking…"  + body
--   expanded (Ctrl+O)               → "▼ reasoning"  + body
--   collapsed                       → "▸ reasoning (Ns)"
local function reasoning_rows(reasoning, body_empty, expanded)
  if reasoning == nil or (reasoning.text or "") == "" then return nil end
  local live = reasoning.streaming and body_empty
  if live or expanded then
    local header_text = live and "▼ thinking…" or "▼ reasoning"
    return tui.column {
      gap = 0,
      children = {
        tui.text { content = header_text, style = STYLE.footer, wrap = "none" },
        tui.text { content = "  " .. (reasoning.text or ""), style = STYLE.reasoning, wrap = "word" },
      },
    }
  end
  local dur = humanize_duration_ms(reasoning.duration_ms)
  local label = dur and ("▸ reasoning (" .. dur .. ")") or "▸ reasoning"
  return tui.text { content = label, style = STYLE.footer, wrap = "none" }
end

-- Per-turn footer: "▣ <model> · <duration>" in HL_FOOTER.
local function turn_footer(entry)
  local model = strip_claude_prefix(entry.model)
  local dur = humanize_duration_ms(entry.duration_ms)
  if model and dur then
    return tui.text { content = "▣ " .. model .. " · " .. dur, style = STYLE.footer, wrap = "none" }
  elseif model then
    return tui.text { content = "▣ " .. model, style = STYLE.footer, wrap = "none" }
  elseif dur then
    return tui.text { content = "▣ " .. dur, style = STYLE.footer, wrap = "none" }
  end
  return nil
end

local function render_assistant_entry(entry, expanded)
  local body_empty = (entry.text or "") == ""
  local rows = compact {
    reasoning_rows(entry.reasoning, body_empty, expanded),
    body_empty and nil or md(entry.text),
    (not entry.streaming) and turn_footer(entry) or nil,
  }
  return tui.column { gap = 0, children = rows }
end

-- Salient input summary for tool collapsed-line.
local function tool_salient(entry)
  local name = entry.name or ""
  local input = entry.input_table or {}
  if name == "Bash" or name == "bash" then return input.command end
  if name == "Read" or name == "Edit" or name == "Write" or name == "MultiEdit" then
    return input.file_path
  end
  if name == "read_file" or name == "write_file" then return input.path end
  if name == "Grep" or name == "Glob" then return input.pattern end
  if name == "spawn_graph" then
    local nodes = input.graph and input.graph.nodes or nil
    if type(nodes) == "table" then
      local n = #nodes
      if n == 1 then return "1 node" end
      return tostring(n) .. " nodes"
    end
  end
  -- Fall back: first short string field, skipping policy-ish names.
  for k, v in pairs(input) do
    local skip = (k == "on_node_failure" or k == "mode" or k == "policy" or k == "strategy")
    if (not skip) and type(v) == "string" then return v end
  end
  return nil
end

local function tool_collapsed(entry)
  local glyph = "▸ "
  local header_style = entry.error and STYLE.tool_error or STYLE.tool_name
  local salient = tool_salient(entry)
  local header = glyph .. (entry.name or "?")
  if salient then
    local trimmed = salient
    if #trimmed > 80 then trimmed = trimmed:sub(1, 77) .. "..." end
    header = header .. "(" .. trimmed .. ")"
  end
  if entry.output == nil and not entry.error then
    header = header .. " …"  -- running indicator
  end
  local rows = { tui.text { content = header, style = header_style, wrap = "none" } }
  if entry.error then
    rows[#rows + 1] = tui.text { content = "  error", style = STYLE.status_danger, wrap = "none" }
  end
  return tui.column { gap = 0, children = rows }
end

local function tool_expanded(entry)
  local glyph = "▼ "
  local header_style = entry.error and STYLE.tool_error or STYLE.tool_name
  local salient = tool_salient(entry)
  local header = glyph .. (entry.name or "?")
  if salient then
    local trimmed = salient
    if #trimmed > 80 then trimmed = trimmed:sub(1, 77) .. "..." end
    header = header .. "(" .. trimmed .. ")"
  end
  local rows = { tui.text { content = header, style = header_style, wrap = "none" } }
  rows[#rows + 1] = tui.text { content = "  input:",  style = STYLE.footer, wrap = "none" }
  if entry.input and #entry.input > 0 then
    rows[#rows + 1] = tui.text {
      content = "  " .. entry.input,
      style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap = "word",
    }
  end
  if entry.output == nil and not entry.error then
    rows[#rows + 1] = tui.text { content = "  running...", style = STYLE.footer, wrap = "none" }
  else
    rows[#rows + 1] = tui.text { content = "  output:", style = STYLE.footer, wrap = "none" }
    if entry.output and #entry.output > 0 then
      rows[#rows + 1] = tui.text {
        content = "  " .. entry.output,
        style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
        wrap = "word",
      }
    end
  end
  return tui.column { gap = 0, children = rows }
end

local function render_entry(entry, _i, expanded)
  if entry.kind == "tool_call" then
    if expanded then return tool_expanded(entry) end
    return tool_collapsed(entry)
  end
  if entry.role == "assistant" or entry.kind == "stream" then
    return render_assistant_entry(entry, expanded)
  end
  if entry.role == "user" then
    return render_user_entry(entry)
  end
  if entry.role == "system" then
    return tui.text {
      content = "[" .. (entry.text or "") .. "]",
      style   = STYLE.system,
      wrap    = "word",
    }
  end
  return tui.text { content = entry.text or "", wrap = "word" }
end

------------------------------------------------------------------------
-- streaming indicator
------------------------------------------------------------------------

-- Spec section 6: pre-first-delta placeholder is `[thinking... Ns]`,
-- static (no spinner) but with per-second elapsed counter. We use
-- `tui.now_ms()` to compute the elapsed seconds on every render — same
-- monotonic clock the engine ticks on dispatch, so the counter advances
-- whenever a new event lands. Plus a `tui.animation` keeps the render
-- loop alive at frame rate so the counter ticks even between events.
local THINKING_FRAMES = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" }

local function thinking_widget(state)
  if not state.pending then return nil end
  if state.in_flight ~= nil then return nil end
  local elapsed_ms = state.turn_started_at and (tui.now_ms() - state.turn_started_at) or 0
  local secs = math.floor(elapsed_ms / 1000)
  local body = secs > 0
    and string.format("[thinking... %ds]", secs)
    or  "[thinking...]"
  return tui.row {
    gap = 1,
    children = {
      tui.animation {
        frames      = THINKING_FRAMES,
        duration_ms = 800,
      },
      tui.text { content = body, style = STYLE.system, wrap = "none" },
    },
  }
end

------------------------------------------------------------------------
-- statusline
------------------------------------------------------------------------

local function ctx_bar(used, max)
  if used == nil or max == nil or max == 0 then return nil end
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

local function auth_segment(auth)
  if auth == nil then return nil end
  local entries = {}
  for name, status in pairs(auth) do
    entries[#entries + 1] = { name = name, status = status }
  end
  if #entries == 0 then return nil end
  table.sort(entries, function(a, b) return a.name < b.name end)
  local spans = {}
  local shown = 0
  local extra = 0
  for _, e in ipairs(entries) do
    if shown >= 3 then
      extra = extra + 1
    else
      if shown > 0 then
        spans[#spans + 1] = { text = " ", fg = C.status_dim }
      end
      spans[#spans + 1] = { text = e.name .. ":", fg = C.status_dim }
      local marker, color
      if e.status == "connected" then
        marker, color = "✓", C.system
      elseif e.status == "login_required" then
        marker, color = "?", C.status_warn
      elseif e.status == nil then
        marker, color = "·", C.status_dim
      else
        marker, color = "!", C.status_danger
      end
      spans[#spans + 1] = { text = marker, fg = color }
      shown = shown + 1
    end
  end
  if extra > 0 then
    spans[#spans + 1] = { text = " +" .. tostring(extra), fg = C.status_dim }
  end
  return { spans = spans }
end

local function build_statusline_segments(state)
  local segs = {}
  if state.gate_yolo then
    segs[#segs + 1] = { spans = { { text = "YOLO", fg = C.status_danger, bold = true } } }
  end
  local model = strip_claude_prefix(state.model or (state.stats and state.stats.model))
  if model then
    segs[#segs + 1] = { spans = { { text = model, fg = C.system } } }
  else
    segs[#segs + 1] = { spans = { { text = "Start chatting to see stats", fg = C.status_dim } } }
  end

  local s = state.stats or {}
  local last_ctx = s.last_turn_context_tokens or s.context_tokens or s.prompt_tokens
  if last_ctx and state.max_tokens then
    local cb = ctx_bar(last_ctx, state.max_tokens)
    if cb then segs[#segs + 1] = cb end
  end

  if s.cost_usd ~= nil then
    segs[#segs + 1] = { spans = { { text = string.format("$%.2f", s.cost_usd), fg = C.system } } }
  end
  if s.turns ~= nil then
    segs[#segs + 1] = { spans = { { text = tostring(s.turns) .. " turns", fg = C.system } } }
  end
  local last_dur = s.last_turn_duration_ms or state.last_turn_duration_ms or s.duration_ms
  if last_dur ~= nil then
    segs[#segs + 1] = { spans = { { text = humanize_duration_ms(last_dur), fg = C.system } } }
  end
  -- "[done in Xms]" indicator after the most recent stream end.
  if state.last_turn_duration_ms ~= nil and not state.pending and state.in_flight == nil then
    segs[#segs + 1] = {
      spans = {
        { text = "[done in " .. humanize_duration_ms(state.last_turn_duration_ms) .. "]",
          fg = C.status_ok },
      },
    }
  end

  -- Speed: tok/s when both output_tokens and duration are known.
  local ot = s.last_turn_output_tokens or s.completion_tokens
  if ot and last_dur and last_dur > 0 then
    local tps = math.floor((ot * 1000) / last_dur + 0.5)
    segs[#segs + 1] = { spans = { { text = tostring(tps) .. " tok/s", fg = C.system } } }
  end

  local auth = auth_segment(state.auth)
  if auth then segs[#segs + 1] = auth end

  return segs
end

local function statusline(state)
  local segs = build_statusline_segments(state)
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

------------------------------------------------------------------------
-- popups (help, info, warning, error, model_picker, tool_permission, toast)
------------------------------------------------------------------------

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
  /yolo /safe  toggle tool permission gate
  /login /logout  provider auth
  /model       list/switch model
  /resume      resume a previous session
  /dag-test    submit a test DAG]]

local function popup_help(state)
  if not state.popup or state.popup.variant ~= "help" then return nil end
  return tui.anchored {
    anchor = "center",
    width  = "60%",
    height = "60%",
    child  = bordered_popup(
      "popup_help",
      tui.padding {
        value = 1,
        child = tui.column {
          gap = 1,
          children = {
            tui.text { content = "── help ──", style = STYLE.popup_user },
            tui.text { content = HELP_BODY, wrap = "word" },
            tui.text { content = "(Esc / Q / Enter to close)", style = STYLE.status_dim },
          },
        },
      },
      STYLE.popup_user
    ),
  }
end

local function popup_message(state)
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
  return tui.anchored {
    anchor = "center",
    width  = "60%",
    height = "50%",
    child  = bordered_popup(
      "popup_message",
      tui.padding {
        value = 1,
        child = tui.column {
          gap = 1,
          children = compact {
            tui.text { content = title, style = title_style },
            tui.markdown { source = state.popup.body or "", theme = MARKDOWN_THEME, wrap = "word" },
            state.popup.source and tui.text {
              content = "from: " .. state.popup.source,
              style   = STYLE.footer,
            } or nil,
            tui.text { content = "Esc / Q to close", style = STYLE.status_dim },
          },
        },
      },
      border_style
    ),
  }
end

-- Tool permission popup (spec section 12). Border is HL_STATUS_WARN;
-- footer reads `[A]pprove [D]eny (ESC = deny)`. Keyhandlers in update.
local function popup_tool_permission(state)
  if not state.popup or state.popup.variant ~= "tool_permission" then return nil end
  return tui.anchored {
    anchor = "center",
    width  = "60%",
    height = "50%",
    child  = bordered_popup(
      "popup_tool_permission",
      tui.padding {
        value = 1,
        child = tui.column {
          gap = 1,
          children = {
            tui.text {
              content = " permission requested · " .. (state.popup.tool or "?"),
              style   = STYLE.popup_warn,
            },
            tui.text { content = state.popup.body or "", wrap = "word" },
            tui.text {
              content = "[A]pprove   [D]eny   (Esc = deny)",
              style   = STYLE.status_warn,
            },
          },
        },
      },
      STYLE.popup_warn
    ),
  }
end

-- Toast: bottom-left, single-line, no border, in HL_STATUS_INFO. Auto-
-- dismisses on `expires_at_ms` via the per-event prune cycle.
local function popup_toast(state)
  if not state.toast then return nil end
  if state.toast.expires_at_ms and tui.now_ms() >= state.toast.expires_at_ms then
    return nil
  end
  return tui.anchored {
    anchor = "bottom-left",
    offset_x = 1,
    width  = 40,
    child  = tui.text { content = state.toast.text or "", style = STYLE.toast, wrap = "none" },
  }
end

------------------------------------------------------------------------
-- slash autocomplete (inline above input, NOT centered overlay)
------------------------------------------------------------------------

local function slash_autocomplete_inline(state)
  if not state.slash then return nil end
  local matches = state.slash.matches or {}
  if #matches == 0 then
    return tui.text {
      content = "no matching commands",
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  -- Up to 8 rows.
  local cap = 8
  local cursor = state.slash.cursor or 1
  local first = math.max(1, math.min(cursor - cap + 1, #matches - cap + 1))
  if first < 1 then first = 1 end
  local last = math.min(first + cap - 1, #matches)
  local children = {}
  for i = first, last do
    local cmd = matches[i]
    local is_cursor = (i == cursor)
    local row = string.format("/%-12s  %s", cmd.name, cmd.hint or "")
    children[#children + 1] = tui.text {
      content = row,
      style   = is_cursor and { fg = "#000000", bg = C.user } or nil,
      wrap    = "none",
    }
  end
  return tui.column { gap = 0, children = children }
end

------------------------------------------------------------------------
-- DAG panel (sidebar widget)
------------------------------------------------------------------------

local DAG_GLYPHS = {
  pending = "○",
  running = "●",
  done    = "✓",
  error   = "✗",
  skipped = "⊘",
}

local DAG_NODE_STYLE = {
  pending = STYLE.dag_pending,
  running = STYLE.dag_running,
  done    = STYLE.dag_done,
  error   = STYLE.dag_error,
  skipped = STYLE.dag_skipped,
}

local function sorted_keys(m)
  local out = {}
  for k in pairs(m) do out[#out + 1] = k end
  table.sort(out)
  return out
end

local function fmt_elapsed_ms(ms)
  if ms == nil then return "" end
  if ms < 1000 then return tostring(ms) .. "ms" end
  return string.format("%.1fs", ms / 1000)
end

local function prune_dag_runs(dag_runs, now_ms)
  if dag_runs == nil then return {} end
  local pruned = nil
  for run_id, run in pairs(dag_runs) do
    if run.completed_at_ms ~= nil
       and (now_ms - run.completed_at_ms) > DAG_LINGER_MS then
      if pruned == nil then
        pruned = {}
        for k, v in pairs(dag_runs) do pruned[k] = v end
      end
      pruned[run_id] = nil
    end
  end
  return pruned or dag_runs
end

local function dag_run_header(run)
  local short = run.run_id and run.run_id:sub(1, 8) or "?"
  local total = run.total_nodes or 0
  local nodes = run.nodes or {}
  local done = 0
  local nodes_count = 0
  for _, n in pairs(nodes) do
    nodes_count = nodes_count + 1
    if n.status == "done" or n.status == "error" or n.status == "skipped" then
      done = done + 1
    end
  end
  if nodes_count > total then total = nodes_count end
  local title = string.format("DAG %s (%d/%d)", short, done, total)
  return tui.text { content = title, style = STYLE.footer, wrap = "none" }
end

local function dag_node_row(node_id, node, now_ms, narrow)
  local glyph = DAG_GLYPHS[node.status] or "·"
  local style = DAG_NODE_STYLE[node.status] or STYLE.status_dim
  local elapsed
  if node.status == "running" then
    elapsed = now_ms - (node.started_at_ms or now_ms)
  elseif node.status == "done" or node.status == "error" then
    if node.finished_at_ms ~= nil then
      elapsed = node.finished_at_ms - (node.started_at_ms or node.finished_at_ms)
    end
  end
  local elapsed_str = elapsed and (" " .. fmt_elapsed_ms(elapsed)) or ""
  local text
  if narrow then
    text = glyph .. " " .. node_id .. elapsed_str
  else
    local reasoner = node.reasoner or ""
    local status_word = node.status or "?"
    text = string.format("%s %s  %s  %s%s",
      glyph, node_id, reasoner, status_word, elapsed_str)
  end
  return tui.text { content = text, style = style, wrap = "none" }
end

local function dag_panel_children(state, now_ms, narrow)
  local children = {}
  local run_ids = sorted_keys(state.dag_runs)
  for i, run_id in ipairs(run_ids) do
    if i > 1 then
      children[#children + 1] = tui.text { content = "", wrap = "none" }
    end
    local run = state.dag_runs[run_id]
    children[#children + 1] = dag_run_header(run)
    local node_ids = sorted_keys(run.nodes or {})
    for _, node_id in ipairs(node_ids) do
      children[#children + 1] = dag_node_row(node_id, run.nodes[node_id], now_ms, narrow)
    end
  end
  if #children == 0 then
    children[#children + 1] = tui.text {
      content = "(no active runs)",
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  return children
end

local function dag_panel(state)
  local narrow = true
  local now_ms = tui.now_ms()
  local children = {
    tui.text { content = "Graph", style = STYLE.footer, wrap = "none" },
    tui.text { content = string.rep("─", 30), style = STYLE.footer, wrap = "none" },
  }
  for _, c in ipairs(dag_panel_children(state, now_ms, narrow)) do
    children[#children + 1] = c
  end
  return tui.constrained {
    min_width = 28,
    max_width = 36,
    child = tui.padding {
      value = 1,
      child = tui.column { gap = 0, children = children },
    },
  }
end

local function vertical_separator()
  return tui.constrained {
    min_width = 1,
    max_width = 1,
    child = tui.text { content = "│", style = STYLE.dag_separator, wrap = "none" },
  }
end

------------------------------------------------------------------------
-- view
------------------------------------------------------------------------

local function transcript(state)
  local entries = state.entries or {}
  local widgets = {}
  for i, e in ipairs(entries) do
    widgets[#widgets + 1] = render_entry(e, i, state.expanded_details)
  end
  -- Append thinking indicator inline at the bottom when pending.
  local think = thinking_widget(state)
  if think then widgets[#widgets + 1] = think end
  return tui.scrollable {
    key       = "transcript",
    stick_to  = "end",
    scrollbar = "auto",
    child     = tui.column { gap = 1, children = widgets },
  }
end

local function view(state)
  local body_row = tui.row {
    gap = 0,
    children = compact {
      tui.expanded { child = transcript(state) },
      state.show_sidebar and vertical_separator() or nil,
      state.show_sidebar and dag_panel(state)        or nil,
    },
  }

  -- Input field with full-width rounded border per legacy spec section
  -- 7. The `tui.text_input` is the bare control; `bordered_box` wraps
  -- it in `╭─╮ │ ╰─╯` chrome so the input visually matches user
  -- message blocks. Border colour brightens (HL_USER) when the input
  -- is focused; dims to HL_STATUS_DIM when a popup steals focus.
  local input_focused = state.focused_id == "input"
    and not (state.popup and state.popup.variant == "tool_permission")
  local input_border_style = input_focused
    and STYLE.input_border
    or STYLE.input_border_unfocused
  local input_field = bordered_box(
    tui.text_input {
      key       = "input",
      value     = state.input_value,
      -- A popup that wants to absorb single-char keys (tool permission
      -- with [A]/[D]) needs the input to drop focus while open; the
      -- engine routes editing keys only to a `focused = true` input,
      -- so single chars otherwise vanish into the buffer.
      focused   = input_focused,
      on_change = "input.changed",
      on_submit = "input.submit",
      min_lines = 1,
      max_lines = 6,
      placeholder = "type a message — Enter to send, /help for keys, /quit to exit",
    },
    input_border_style,
    -- Stable user-key on the bordered_box's outer column so the
    -- reconciler reuses the text_input instance (preserving cursor +
    -- selection + scroll) across renders that change main_column's
    -- child count — notably when the slash autocomplete dropdown
    -- appears or vanishes between the body row and the input.
    "input-field"
  )

  -- Layout (top → bottom): transcript | slash autocomplete (when open) |
  -- input row | statusline. Statusline lives BELOW the input per legacy
  -- spec — pushing it above the input visibly inverts the screen weight,
  -- making the input feel like a status row rather than the primary
  -- focus surface.
  local main_column = tui.column {
    gap = 0,
    children = compact {
      tui.expanded { child = body_row },
      slash_autocomplete_inline(state),
      input_field,
      statusline(state),
    },
  }

  -- 1-cell outer padding so the UI doesn't sit flush against the
  -- terminal edges — gives content breathing room on every side without
  -- baking the layout decision into the engine.
  return tui.padding {
    value = 1,
    child = tui.stack {
      children = compact {
        main_column,
        popup_help(state),
        popup_message(state),
        popup_tool_permission(state),
        popup_toast(state),
      },
    },
  }
end

------------------------------------------------------------------------
-- transcript helpers (state mutation)
------------------------------------------------------------------------

local function push_entry(state, entry)
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  entries[#entries + 1] = entry
  return shallow_merge(state, { entries = entries })
end

local function append_assistant_delta(state, delta)
  if state.in_flight ~= nil and state.entries[state.in_flight] then
    local entries = {}
    for i, v in ipairs(state.entries) do
      entries[i] = (i == state.in_flight)
        and shallow_merge(v, { text = (v.text or "") .. delta, streaming = true })
        or v
    end
    return shallow_merge(state, { entries = entries, pending = false })
  end
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  entries[#entries + 1] = {
    role = "assistant", text = delta, kind = "stream", streaming = true,
  }
  return shallow_merge(state, {
    entries   = entries,
    in_flight = #entries,
    pending   = false,
  })
end

local function append_reasoning_delta(state, delta)
  -- Ensure we have an in-flight assistant entry; reasoning rides above it.
  local idx = state.in_flight
  local entries = {}
  for i, v in ipairs(state.entries) do entries[i] = v end
  if idx == nil then
    entries[#entries + 1] = {
      role = "assistant", text = "", kind = "stream", streaming = true,
      reasoning = { text = delta, streaming = true },
    }
    return shallow_merge(state, {
      entries = entries, in_flight = #entries, pending = false,
    })
  end
  local cur = entries[idx]
  local prev = cur.reasoning or { text = "", streaming = true }
  entries[idx] = shallow_merge(cur, {
    streaming = true,
    reasoning = shallow_merge(prev, {
      text      = (prev.text or "") .. delta,
      streaming = true,
    }),
  })
  return shallow_merge(state, { entries = entries, pending = false })
end

local function finalize_reasoning(state, duration_ms)
  if state.in_flight == nil then return state end
  local entries = {}
  for i, v in ipairs(state.entries) do
    if i == state.in_flight then
      local prev = v.reasoning or { text = "", streaming = true }
      entries[i] = shallow_merge(v, {
        reasoning = shallow_merge(prev, {
          streaming   = false,
          duration_ms = duration_ms or prev.duration_ms,
        }),
      })
    else
      entries[i] = v
    end
  end
  return shallow_merge(state, { entries = entries })
end

local function finalize_assistant(state, final_text, model, duration_ms)
  local now = tui.now_ms()
  local turn_dur = duration_ms or (state.turn_started_at and (now - state.turn_started_at)) or nil
  if state.in_flight == nil then
    -- Empty turn (e.g. error) — still record durations.
    return shallow_merge(state, {
      pending = false, turn_started_at = NIL_SENTINEL,
      last_turn_duration_ms = turn_dur,
    })
  end
  local entries = {}
  for i, v in ipairs(state.entries) do
    if i == state.in_flight then
      entries[i] = shallow_merge(v, {
        text        = final_text and #final_text > 0 and final_text or v.text,
        model       = model or v.model,
        duration_ms = duration_ms or v.duration_ms,
        streaming   = false,
      })
    else
      entries[i] = v
    end
  end
  return shallow_merge(state, {
    entries          = entries,
    in_flight        = NIL_SENTINEL,
    pending          = false,
    turn_started_at  = NIL_SENTINEL,
    last_turn_duration_ms = turn_dur,
  })
end

local function attach_tool_end(state, id, output, error_flag)
  local entries = {}
  local matched = false
  for i, v in ipairs(state.entries) do
    if not matched and v.kind == "tool_call" and v.id == id then
      entries[i] = shallow_merge(v, { output = output or "", error = error_flag })
      matched = true
    else
      entries[i] = v
    end
  end
  if not matched then return state end
  return shallow_merge(state, { entries = entries })
end

------------------------------------------------------------------------
-- DAG panel state mutators
------------------------------------------------------------------------

local function dag_apply(state, run_id, fn)
  local prev_runs = state.dag_runs or {}
  local new_runs = {}
  for k, v in pairs(prev_runs) do new_runs[k] = v end
  new_runs[run_id] = fn(prev_runs[run_id])
  return shallow_merge(state, { dag_runs = new_runs })
end

local function dag_run_started(state, run_id, total_nodes, now_ms)
  if state.dag_runs and state.dag_runs[run_id] then return state end
  return dag_apply(state, run_id, function(_)
    return {
      run_id = run_id, total_nodes = total_nodes or 0,
      started_at_ms = now_ms, nodes = {},
      completed_at_ms = nil, status = nil,
    }
  end)
end

local function dag_node_dispatched(state, run_id, node_id, reasoner, now_ms)
  return dag_apply(state, run_id, function(prev)
    local run = prev or {
      run_id = run_id, total_nodes = 0, started_at_ms = now_ms,
      nodes = {}, completed_at_ms = nil,
    }
    local nodes = {}
    for k, v in pairs(run.nodes or {}) do nodes[k] = v end
    nodes[node_id] = {
      reasoner = reasoner or "",
      status = "running",
      started_at_ms = now_ms,
      finished_at_ms = nil,
    }
    return shallow_merge(run, { nodes = nodes })
  end)
end

local function dag_node_result(state, run_id, node_id, has_output, has_error, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]
          and state.dag_runs[run_id].nodes
          and state.dag_runs[run_id].nodes[node_id]) then
    return state
  end
  return dag_apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[node_id]
    local status
    if has_output then status = "done"
    elseif has_error then status = "error"
    else status = "error" end
    nodes[node_id] = shallow_merge(node, {
      status = status, finished_at_ms = now_ms,
    })
    return shallow_merge(prev, { nodes = nodes })
  end)
end

local function dag_run_complete(state, run_id, status, results, now_ms)
  if not (state.dag_runs and state.dag_runs[run_id]) then return state end
  return dag_apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    if type(results) == "table" then
      for node_id, entry in pairs(results) do
        if type(entry) == "table" and entry.skipped == true then
          nodes[node_id] = {
            reasoner = nodes[node_id] and nodes[node_id].reasoner or "",
            status = "skipped",
            started_at_ms = nodes[node_id] and nodes[node_id].started_at_ms or now_ms,
            finished_at_ms = now_ms,
          }
        end
      end
    end
    return shallow_merge(prev, {
      nodes = nodes, completed_at_ms = now_ms, status = status,
    })
  end)
end

------------------------------------------------------------------------
-- max-tokens lookup (per-model context window)
------------------------------------------------------------------------

local function model_max_tokens(model)
  if model == nil then return nil end
  local m = model:lower()
  if m:find("opus") or m:find("sonnet") or m:find("haiku") then return 200000 end
  return nil
end

------------------------------------------------------------------------
-- update
------------------------------------------------------------------------

local function parse_slash(text)
  if text:sub(1, 1) ~= "/" then return nil, nil, false end
  local cmd, rest = text:match("^/(%S+)%s*(.*)$")
  local has_ws = text:find("^/%S+%s") ~= nil
  return cmd, (rest ~= "" and rest or nil), has_ws
end

local function refresh_slash(state, text)
  if text == nil or text:sub(1, 1) ~= "/" then
    return shallow_merge(state, { slash = NIL_SENTINEL })
  end
  local cmd, _args, has_ws = parse_slash(text)
  if has_ws then
    -- User typed past the command name → close the popup.
    return shallow_merge(state, { slash = NIL_SENTINEL })
  end
  local matches = slash_filter(cmd or "")
  return shallow_merge(state, {
    slash = { matches = matches, cursor = 1, query = cmd or "" },
  })
end

local function update(msg, state)
  local kind = msg.kind or ""

  -- Pure-update prune for stale dag runs + expired toast.
  do
    local now = tui.now_ms()
    local pruned = prune_dag_runs(state.dag_runs or {}, now)
    if pruned ~= state.dag_runs then
      state = shallow_merge(state, { dag_runs = pruned })
    end
    if state.toast and state.toast.expires_at_ms and now >= state.toast.expires_at_ms then
      state = shallow_merge(state, { toast = NIL_SENTINEL })
    end
  end

  -- ── text_input callbacks ────────────────────────────────────────────
  if kind == "input.changed" then
    local v = msg.value or ""
    state = shallow_merge(state, { input_value = v })
    state = refresh_slash(state, v)
    return state, {}
  end

  if kind == "input.submit" then
    local text = msg.value or ""
    if #text == 0 then return state, {} end
    -- Slash dispatch.
    local cmd, args, _has_ws = parse_slash(text)
    if cmd == "quit" or cmd == "exit" then
      return state, { { kind = "exit" } }
    end
    if cmd == "new" or cmd == "clear" then
      local cleared = shallow_merge(state, {
        entries = {}, in_flight = NIL_SENTINEL, input_value = "",
        pending = false, slash = NIL_SENTINEL,
      })
      return cleared, {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.reset" } },
      }
    end
    if cmd == "help" then
      return shallow_merge(state, {
        input_value = "", slash = NIL_SENTINEL,
        popup = { variant = "help" },
      }), {}
    end
    if cmd == "yolo" then
      local s = shallow_merge(state, { input_value = "", slash = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "yolo" } },
      }
    end
    if cmd == "safe" then
      local s = shallow_merge(state, { input_value = "", slash = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "normal" } },
      }
    end
    if cmd == "login" or cmd == "logout" then
      local body = { kind = "chat." .. cmd .. "_requested" }
      if args and #args > 0 then body.provider = args end
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine", body = body },
      }
    end
    if cmd == "model" then
      local body
      if args and #args > 0 then
        body = { kind = "chat.model.set", model = args }
      else
        body = { kind = "chat.model.list_requested" }
      end
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine", body = body },
      }
    end
    if cmd == "resume" then
      local body = { kind = "chat.resume" }
      if args and #args > 0 then body.session_id = args end
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine", body = body },
      }
    end
    if cmd == "dag-test" then
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.command", name = "dag-test" } },
      }
    end
    if cmd ~= nil then
      -- Unknown slash → generic chat.command for user-defined Lua handlers.
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.command", name = cmd, args = args or "" } },
      }
    end
    -- Plain text submit.
    local with_user = push_entry(state, { role = "user", text = text, kind = "text" })
    local cleared = shallow_merge(with_user, {
      input_value = "", pending = true,
      turn_started_at = tui.now_ms(), slash = NIL_SENTINEL,
    })
    return cleared, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = text } },
    }
  end

  -- ── keyboard shortcuts ──────────────────────────────────────────────
  -- Ctrl+C and Ctrl+D both exit. Raw-mode terminals deliver these as
  -- key events (not signals), so the app must terminate explicitly.
  if kind == "key.ctrl_c" or kind == "key.ctrl_d" then
    return state, { { kind = "exit" } }
  end

  if kind == "key.ctrl_b" then
    return shallow_merge(state, { show_sidebar = not state.show_sidebar }), {}
  end

  if kind == "key.ctrl_o" then
    -- Global toggle for tool I/O + reasoning expansion.
    return shallow_merge(state, { expanded_details = not state.expanded_details }), {}
  end

  if kind == "key.?" or kind == "key.shift_?" then
    -- ? opens help only when the input is empty (so users can type ? in
    -- regular messages). Otherwise it bubbles into the input field.
    if state.input_value == "" then
      return shallow_merge(state, { popup = { variant = "help" } }), {}
    end
    return state, {}
  end

  if kind == "key.escape" then
    -- 1) close popup
    if state.popup or state.toast then
      -- Tool permission ESC = deny.
      if state.popup and state.popup.variant == "tool_permission" then
        local id = state.popup.id
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = { kind = "tool.permission_response", id = id, decision = "deny" } },
        }
      end
      return shallow_merge(state, { popup = NIL_SENTINEL, toast = NIL_SENTINEL }), {}
    end
    -- 2) close slash autocomplete
    if state.slash then
      return shallow_merge(state, { slash = NIL_SENTINEL }), {}
    end
    -- 3) double-ESC escalation
    local now = tui.now_ms()
    if state.last_esc_ms and (now - state.last_esc_ms) <= DOUBLE_ESC_MS then
      return shallow_merge(state, { last_esc_ms = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt_all" } },
      }
    end
    -- 4) single ESC interrupts the current turn
    if state.pending or state.in_flight ~= nil then
      return shallow_merge(state, { last_esc_ms = now }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt" } },
      }
    end
    -- Stamp anyway so a follow-up ESC within the window can escalate.
    return shallow_merge(state, { last_esc_ms = now }), {}
  end

  -- Tool permission popup keys.
  if state.popup and state.popup.variant == "tool_permission" then
    if kind == "key.a" or kind == "key.A" then
      local id = state.popup.id
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "approve" } },
      }
    end
    if kind == "key.d" or kind == "key.D" then
      local id = state.popup.id
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "deny" } },
      }
    end
  end

  -- Slash autocomplete keys (when slash popup open).
  if state.slash then
    if kind == "key.up" then
      local n = #(state.slash.matches or {})
      if n == 0 then return state, {} end
      local cur = state.slash.cursor or 1
      cur = cur - 1
      if cur < 1 then cur = n end
      return shallow_merge(state, {
        slash = shallow_merge(state.slash, { cursor = cur }),
      }), {}
    end
    if kind == "key.down" then
      local n = #(state.slash.matches or {})
      if n == 0 then return state, {} end
      local cur = state.slash.cursor or 1
      cur = cur + 1
      if cur > n then cur = 1 end
      return shallow_merge(state, {
        slash = shallow_merge(state.slash, { cursor = cur }),
      }), {}
    end
    if kind == "key.tab" then
      local m = state.slash.matches and state.slash.matches[state.slash.cursor]
      if m then
        local v = "/" .. m.name .. (m.takes_args and " " or "")
        return shallow_merge(state, { input_value = v, slash = NIL_SENTINEL }), {}
      end
    end
  end

  -- Scroll keys: route to the active popup's scrollable when a popup is
  -- open; otherwise to the transcript. The popup's body is wrapped in
  -- `tui.scrollable { key = "popup_<variant>" }` (see `bordered_popup`),
  -- so the same `tui.scroll_*` API drives both. Disabling transcript
  -- scrolling while a popup is up matches the user-expected "modal
  -- focus" gesture — the popup owns the keyboard while it's visible.
  local function active_scroll_key()
    if state.popup then
      local v = state.popup.variant
      if v == "help" then return "popup_help" end
      if v == "info" or v == "warning" or v == "error" then return "popup_message" end
      if v == "tool_permission" then return "popup_tool_permission" end
    end
    return nil
  end

  if kind == "key.pageup" then
    local target = active_scroll_key()
    if target then
      tui.scroll_by(target, -10)
    else
      tui.scroll_by("transcript", -10)
    end
    return state, {}
  end
  if kind == "key.pagedown" then
    local target = active_scroll_key()
    if target then
      tui.scroll_by(target, 10)
    else
      tui.scroll_by("transcript", 10)
    end
    return state, {}
  end
  -- Up/Down arrows scroll the active surface by one line — Mac keyboards
  -- don't have PgUp/PgDn, so arrows are the muscle-memory equivalent.
  -- The text_input router only bubbles arrows when the cursor sits at
  -- an edge of the input's content (single-line, or first/last visual
  -- row of multi-line), so this only fires when the input has nowhere
  -- to move the cursor.
  if kind == "key.up" then
    local target = active_scroll_key()
    if target then
      tui.scroll_by(target, -1)
    else
      tui.scroll_by("transcript", -1)
    end
    return state, {}
  end
  if kind == "key.down" then
    local target = active_scroll_key()
    if target then
      tui.scroll_by(target, 1)
    else
      tui.scroll_by("transcript", 1)
    end
    return state, {}
  end
  if kind == "key.home" then
    local target = active_scroll_key()
    if target then
      tui.scroll_to(target, 0)
    else
      tui.scroll_to("transcript", 0)
    end
    return state, {}
  end
  if kind == "key.end" then
    local target = active_scroll_key()
    if target then
      tui.scroll_into_view(target)
    else
      tui.scroll_into_view("transcript")
    end
    return state, {}
  end

  -- ── inbound chat-contract events ────────────────────────────────────
  if kind == "chat.message.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    return push_entry(state, {
      role = msg.role or "system", text = text, kind = "text",
    }), {}
  end

  if kind == "chat.stream.delta" then
    local t = msg.text or msg.delta or ""
    if #t == 0 then return state, {} end
    return append_assistant_delta(state, t), {}
  end

  if kind == "chat.stream.end" then
    return finalize_assistant(state, msg.text, msg.model, msg.duration_ms), {}
  end

  if kind == "chat.stream.reasoning_delta" then
    local t = msg.text or msg.delta or ""
    if #t == 0 then return state, {} end
    return append_reasoning_delta(state, t), {}
  end

  if kind == "chat.stream.reasoning_end" then
    return finalize_reasoning(state, msg.duration_ms), {}
  end

  if kind == "chat.session.stats" then
    local stats = shallow_merge(state.stats or {}, {})
    for k, v in pairs(msg) do
      if k ~= "kind" then stats[k] = v end
    end
    local s = shallow_merge(state, { stats = stats })
    if msg.model and not state.model then
      s = shallow_merge(s, {
        model = msg.model,
        max_tokens = model_max_tokens(msg.model) or state.max_tokens,
      })
    end
    return s, {}
  end

  if kind == "chat.tool.start" then
    -- Preserve the raw input table for `tool_salient`.
    local input_str
    if type(msg.input) == "string" then input_str = msg.input
    elseif type(msg.input) == "table" then input_str = "(object)"
    else input_str = "" end
    return push_entry(state, {
      kind   = "tool_call",
      role   = "tool",
      id     = msg.id or "",
      name   = msg.name or "?",
      input  = input_str,
      input_table = type(msg.input) == "table" and msg.input or nil,
    }), {}
  end

  if kind == "chat.tool.end" then
    return attach_tool_end(state, msg.id or "", msg.output or "", msg.error == true), {}
  end

  if kind == "chat.popup" then
    local v = msg.level or "info"
    return shallow_merge(state, {
      popup = {
        variant = v,
        title   = msg.title or v,
        body    = msg.message or msg.text or "",
        source  = msg.source,
      },
    }), {}
  end

  if kind == "chat.toast" then
    local now = tui.now_ms()
    local ttl = msg.ttl_ms or 2000
    return shallow_merge(state, {
      toast = { text = msg.text or "", expires_at_ms = now + ttl },
    }), {}
  end

  if kind == "chat.model.set_ack" then
    return shallow_merge(state, {
      model = msg.model or state.model,
      max_tokens = model_max_tokens(msg.model) or state.max_tokens,
    }), {}
  end

  if kind == "chat.auth.status" then
    local provider = msg.provider or ""
    local status = msg.status or msg.state or "unknown"
    if provider == "" then return state, {} end
    local auth = {}
    for k, v in pairs(state.auth or {}) do auth[k] = v end
    auth[provider] = status
    return shallow_merge(state, { auth = auth }), {}
  end

  if kind == "tool-gate.permission_request" then
    -- args displayed pretty in the body.
    local body
    if type(msg.input) == "table" then
      body = "(JSON args; provide via msg.input_pretty for rich format)"
    else
      body = tostring(msg.input or "")
    end
    return shallow_merge(state, {
      popup = {
        variant = "tool_permission",
        tool    = msg.tool or msg.name or "?",
        id      = msg.id,
        body    = msg.input_pretty or body,
        source  = msg.source,
      },
    }), {}
  end

  if kind == "tool-gate.mode_changed" then
    return shallow_merge(state, { gate_yolo = msg.mode == "yolo" }), {}
  end

  -- DAG observation
  if kind == "graph.run_started" then
    local now = tui.now_ms()
    return dag_run_started(state, msg.run_id or "", msg.total_nodes or 0, now), {}
  end
  if kind == "graph.node_dispatched" then
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    return dag_node_dispatched(state, msg.run_id, msg.node_id, msg.reasoner or "", now), {}
  end
  if kind == "graph.node_result" then
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    local has_output = msg.output ~= nil
    local has_error  = msg.error  ~= nil
    return dag_node_result(state, msg.run_id, msg.node_id, has_output, has_error, now), {}
  end
  if kind == "graph.run_complete" then
    if (msg.run_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    return dag_run_complete(state, msg.run_id, msg.status, msg.results, now), {}
  end

  return state, {}
end

------------------------------------------------------------------------
-- start
------------------------------------------------------------------------

tui.start {
  initial_state = initial_state(),
  view          = view,
  update        = update,
}
