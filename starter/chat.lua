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
--   chat.popup, chat.auth.status, chat.model.set_ack, chat.models.listed,
--   chat.tool.permission_request, tool-gate.mode_changed,
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

-- Pretty-print a Lua table as 2-space-indented JSON-ish text. Used by
-- `tool_expanded` to render `chat.tool.start` `input` payloads in the
-- expanded view, matching legacy spec section 5: "{pretty-printed JSON}
-- (2-space indent, HL_MD_CODE_BLOCK)". Strings are quoted, numbers and
-- booleans render verbatim, nested tables nest one indent level. Arrays
-- and objects are distinguished by whether the table has a numeric `[1]`
-- key (no general way to tell in Lua, but the JSON-decoded shape from
-- nefor-protocol is reliable).
local function pretty_json(value, indent)
  indent = indent or 0
  local pad   = string.rep("  ", indent)
  local pad_n = string.rep("  ", indent + 1)
  local t = type(value)
  if t == "string" then
    return string.format("%q", value)
  end
  if t == "number" or t == "boolean" then
    return tostring(value)
  end
  if t == "nil" then
    return "null"
  end
  if t ~= "table" then
    return string.format("%q", tostring(value))
  end
  -- Distinguish arrays from objects. An array has consecutive integer
  -- keys 1..N and `#value == N`. An object has at least one string key.
  local n = 0
  for _ in pairs(value) do n = n + 1 end
  if n == 0 then
    return "{}"
  end
  local is_array = (#value == n)
  if is_array then
    local parts = {}
    for i = 1, #value do
      parts[i] = pad_n .. pretty_json(value[i], indent + 1)
    end
    return "[\n" .. table.concat(parts, ",\n") .. "\n" .. pad .. "]"
  end
  -- Object: sort keys for stable display.
  local keys = {}
  for k, _ in pairs(value) do
    if type(k) == "string" then keys[#keys + 1] = k end
  end
  table.sort(keys)
  local parts = {}
  for _, k in ipairs(keys) do
    parts[#parts + 1] = string.format("%s%q: %s",
      pad_n, k, pretty_json(value[k], indent + 1))
  end
  return "{\n" .. table.concat(parts, ",\n") .. "\n" .. pad .. "}"
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

-- Pad every line of `text` with trailing spaces to the longest line's
-- width so a styled bg renders as a rectangle instead of ragging out
-- to per-line content widths. Lines are rendered with `wrap = "none"`
-- by callers when this matters; padding only fixes the natural-line
-- raggedness, not post-wrap raggedness.
local function pad_block(text)
  if type(text) ~= "string" or #text == 0 then return text end
  local lines = {}
  for line in text:gmatch("([^\n]*)") do
    lines[#lines + 1] = line
  end
  -- gmatch leaves a trailing empty match after the last newline; drop it.
  if #lines > 0 and lines[#lines] == "" then
    table.remove(lines, #lines)
  end
  local max_w = 0
  for _, l in ipairs(lines) do
    if #l > max_w then max_w = #l end
  end
  for i, l in ipairs(lines) do
    if #l < max_w then
      lines[i] = l .. string.rep(" ", max_w - #l)
    end
  end
  return table.concat(lines, "\n")
end

-- The spawn_graph tool returns a verbose acknowledgment string:
--   "Submitted sub-graph run_id=<id>. Acknowledge briefly to the user,
--    or chain another tool call. The real result will arrive later as a
--    user message tagged `[spawn_graph(run_id=<id>) result]`."
-- That whole blob is LLM instruction noise. Surface just the run_id —
-- progress is already visible in the DAG sidebar.
local function format_spawn_graph_output(output)
  if type(output) ~= "string" or #output == 0 then return output end
  local run_id = output:match("run_id=([%w%-]+)")
  if run_id then return "submitted as " .. run_id end
  return output
end

-- Render a `spawn_graph` args.graph as a compact node list + edge list.
-- Args of each node are deliberately omitted so the popup stays scannable;
-- a future "focus a node" UI can surface them on demand.
local function format_graph(graph)
  if type(graph) ~= "table" then return tostring(graph) end
  local lines = {}
  local nodes = graph.nodes
  if type(nodes) == "table" and #nodes > 0 then
    lines[#lines + 1] = "nodes:"
    for _, n in ipairs(nodes) do
      local id = n.id or n.name or "?"
      local reasoner = n.reasoner or n.kind or n.type or "?"
      lines[#lines + 1] = "  " .. id .. " (" .. reasoner .. ")"
    end
  end
  local edges = graph.edges
  if type(edges) == "table" and #edges > 0 then
    if #lines > 0 then lines[#lines + 1] = "" end
    lines[#lines + 1] = "edges:"
    for _, e in ipairs(edges) do
      local from = e.from or e.src or e[1] or "?"
      local to   = e.to   or e.dst or e[2] or "?"
      lines[#lines + 1] = "  " .. from .. " -> " .. to
    end
  end
  if #lines == 0 then return "(empty graph)" end
  return table.concat(lines, "\n")
end

-- Pretty-print an args table from a `chat.tool.permission_request` event
-- so the popup body shows a human-legible summary of the call.
-- Stringy values render verbatim; nested tables get a compact `{...}`
-- placeholder rather than a recursive dump (most tools take flat args,
-- and a long nested blob would blow up the popup anyway). The
-- `spawn_graph` tool gets a dedicated graph layout via `format_graph`.
local function format_args(args)
  if args == nil then return "" end
  if type(args) ~= "table" then return tostring(args) end
  -- spawn_graph: the only meaningful field is `graph`, and JSON of it is
  -- noise. Render as a node + edge list per the user's spec.
  if type(args.graph) == "table" then
    return format_graph(args.graph)
  end
  -- Collect string keys (NCP args are JSON objects) in insertion-ish
  -- order. Lua tables don't preserve order; sort for a stable display.
  local keys = {}
  for k, _ in pairs(args) do
    if type(k) == "string" then keys[#keys + 1] = k end
  end
  table.sort(keys)
  if #keys == 0 then
    -- Could be an empty object or an array. ipairs() handles arrays.
    local arr = {}
    for _, v in ipairs(args) do arr[#arr + 1] = tostring(v) end
    if #arr == 0 then return "{}" end
    return "[" .. table.concat(arr, ", ") .. "]"
  end
  local parts = {}
  for _, k in ipairs(keys) do
    local v = args[k]
    local rendered
    if type(v) == "table" then
      rendered = "{...}"
    elseif type(v) == "string" then
      rendered = string.format("%q", v)
    else
      rendered = tostring(v)
    end
    parts[#parts + 1] = k .. " = " .. rendered
  end
  return table.concat(parts, "\n")
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
  bold          = { bold = true },
  italic        = { italic = true },
  code          = { fg = C.md_code_fg, bg = C.md_code_inline_bg },
  code_block    = { fg = C.md_code_fg, bg = C.md_code_block_bg },
  -- Heading hierarchy: filled-to-hollow circle glyphs encode depth
  -- (Emacs org-bullets-style) and a rotating color palette gives each
  -- level a distinct hue. Bold for top three, italic decay for h4-h6
  -- so visual weight tracks importance even though terminals can't
  -- shrink fonts.
  h1 = { prefix = "●", fg = "#ff66cc", bold = true, underline = true },
  h2 = { prefix = "◉", fg = "#66ddff", bold = true },
  h3 = { prefix = "◎", fg = "#88aaff", bold = true },
  h4 = { prefix = "○", fg = "#88dd88", bold = true, italic = true },
  h5 = { prefix = "◌", fg = "#ddcc66", italic = true },
  h6 = { prefix = "·", fg = "#dd9966", italic = true },
  link          = { fg = C.user, underline = true },
  -- Blockquote: the `▎ ` rail glyph + dim cyan italic clearly reads
  -- as "this is a quote" without competing visually with regular prose.
  blockquote    = { fg = "#7faaaa", italic = true },
  list_marker   = { fg = C.user },
  strikethrough = { strikethrough = true },
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
--   prompt_history    list of submitted prompts (newest at index 1, cap 200)
--   history_cursor    nil = not navigating; integer = index into prompt_history

local DAG_LINGER_MS  = 2000
local DOUBLE_ESC_MS  = 600
local HISTORY_CAP    = 200

local function initial_state()
  return {
    entries          = {},
    in_flight        = nil,
    input_value      = "",
    focused_id       = "input",
    show_sidebar     = true,
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
    prompt_history   = {},
    history_cursor   = nil,
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
  -- spawn_graph: render as compact node-list + edge-list. Args of each
  -- node are intentionally omitted (would clutter); future "focus a node"
  -- UI surfaces them on demand. For everything else, prefer JSON pretty-
  -- print of the structured input_table per legacy spec section 5; fall
  -- back to the raw string when only a string was sent.
  local input_text
  if entry.name == "spawn_graph" and entry.input_table
    and type(entry.input_table.graph) == "table" then
    input_text = format_graph(entry.input_table.graph)
  elseif entry.input_table ~= nil then
    input_text = pretty_json(entry.input_table)
  elseif entry.input and #entry.input > 0 and entry.input ~= "(object)" then
    input_text = entry.input
  end
  if input_text and #input_text > 0 then
    -- 2-space indent each line so the body sits inset from the bullet
    -- column and the dark background reads as a single block. Pad each
    -- line to max width post-indent so the bg renders as a rectangle.
    local indented = "  " .. input_text:gsub("\n", "\n  ")
    rows[#rows + 1] = tui.text {
      content = pad_block(indented),
      style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap = "none",
    }
  end
  if entry.output == nil and not entry.error then
    rows[#rows + 1] = tui.text { content = "  running...", style = STYLE.footer, wrap = "none" }
  else
    rows[#rows + 1] = tui.text { content = "  output:", style = STYLE.footer, wrap = "none" }
    if entry.output and #entry.output > 0 then
      local out_text = entry.output
      if entry.name == "spawn_graph" then
        out_text = format_spawn_graph_output(out_text)
      end
      local indented_out = "  " .. out_text:gsub("\n", "\n  ")
      rows[#rows + 1] = tui.text {
        content = pad_block(indented_out),
        style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
        wrap = "none",
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
-- static (no spinner) but with per-second elapsed counter. Legacy
-- chose deliberate minimalism here — no spinner, no frame cycle, just
-- a tick that increments the integer second counter once per second.
--
-- We piggyback on `tui.animation` for its frame-rate side effect (it
-- keeps the render loop alive at ~1Hz so the counter advances even
-- without inbound events) but render zero-width frames, so visually
-- there's no spinner — only the static `[thinking... Ns]` row.
local THINKING_TICK_FRAMES = { "", "" }

local function thinking_widget(state)
  if not state.pending then return nil end
  if state.in_flight ~= nil then return nil end
  local elapsed_ms = state.turn_started_at and (tui.now_ms() - state.turn_started_at) or 0
  local secs = math.floor(elapsed_ms / 1000)
  local body = secs > 0
    and string.format("[thinking... %ds]", secs)
    or  "[thinking...]"
  return tui.row {
    gap = 0,
    children = {
      -- Empty 1-second-period animation drives the redraw loop so the
      -- counter ticks; the rendered cells are blank.
      tui.animation {
        frames      = THINKING_TICK_FRAMES,
        duration_ms = 1000,
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

  -- Speed: tok/s when both output_tokens and duration are known.
  local ot = s.last_turn_output_tokens or s.completion_tokens
  if ot and last_dur and last_dur > 0 then
    local tps = math.floor((ot * 1000) / last_dur + 0.5)
    segs[#segs + 1] = { spans = { { text = tostring(tps) .. " tok/s", fg = C.system } } }
  end

  local auth = auth_segment(state.auth)
  if auth then segs[#segs + 1] = auth end

  -- Scroll percentage segment (legacy spec section 4). Hidden when the
  -- transcript fits the viewport (no scrollback). At-bottom shows
  -- `100% ↓ bottom`; at-top `0% ↑ top`; mid `{pct}% ↑`.
  --
  -- `pcall` because the snapshot map only carries an entry once the
  -- scrollable has been laid out — pre-first-render the call would
  -- raise `no scrollable with key 'transcript'`. We treat any failure
  -- as "no segment yet" rather than letting the statusline blow up.
  local ok, snap = pcall(tui.scroll_position, "transcript")
  if ok and snap and snap.max and snap.max > 0 then
    local offset = snap.offset or 0
    local max = snap.max
    if offset >= max then
      segs[#segs + 1] = { spans = {
        { text = "100% ↓ bottom", fg = C.status_dim },
      } }
    elseif offset <= 0 then
      segs[#segs + 1] = { spans = {
        { text = "0% ↑ top", fg = C.system },
      } }
    else
      local pct = math.floor(100 * (max - offset) / max + 0.5)
      segs[#segs + 1] = { spans = {
        { text = tostring(pct) .. "% ↑", fg = C.system },
      } }
    end
  end

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
  /resume      resume a previous session]]

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

-- Filter the model picker's `models` list against the typed query.
-- Substring match (case-insensitive) against `"<provider> <model>"`.
-- Stable sort order is preserved by walking the source list in order.
local function model_picker_filter(models, query)
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

-- Count entries in the awaiting-set table.
local function awaiting_count(awaiting)
  if awaiting == nil then return 0 end
  local n = 0
  for _, _ in pairs(awaiting) do n = n + 1 end
  return n
end

------------------------------------------------------------------------
-- session resume — picker + bus event
------------------------------------------------------------------------
--
-- `/resume` pops a session picker showing the last 10 sessions on disk
-- (newest first) with a one-line preview of the first user prompt.
-- Selecting a row emits a `sessions.resume_request { session_id }`
-- envelope onto the NCP bus and dismisses the picker — no process exit,
-- no sidechannel file. The starter's `sessions` Lua module subscribes to
-- that kind via `nefor.bus.on_event` and runs the in-process resume
-- sequence (emit `session_end` → swap state → emit `session_start` →
-- replay jsonl → emit `resume_done`). Per-plugin handlers in the
-- agentic_workflow transforms react to the lifecycle events to flush
-- and rebuild their own state. The TUI process stays alive across the
-- whole flip.

-- Resolve the sessions data root. Must match `starter/sessions.lua`'s
-- `data_root()` exactly — picker reads from the same directory the
-- writer writes to. Resolution order, first hit wins:
--   1. `NEFOR_DATA_HOME` — test override + canonical setting.
--   2. `XDG_DATA_HOME/nefor` — standard XDG.
--   3. `$HOME/.local/share/nefor` — XDG default fallback.
-- Earlier the picker resolved to `$HOME/Library/Application Support/nefor`
-- on macOS while the writer used XDG, so the picker showed only stale
-- legacy sessions and never saw new ones. Aligning the two resolvers
-- is the fix.
local function nefor_data_root()
  local override = os.getenv("NEFOR_DATA_HOME")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function session_dir()
  local root = nefor_data_root()
  if root == nil then return nil end
  return root .. "/sessions"
end

-- Extract the value of `"text": "..."` from a session-log JSONL line
-- carrying a chat.input.submit event. The wire shape is:
--   {"ts":"...","origin":"...","payload":"{\"type\":\"event\",\"body\":{\"kind\":\"chat.input.submit\",\"text\":\"<actual>\"}}"}
-- The `text` field lives inside the embedded JSON string of `payload`,
-- so the literal JSONL bytes contain `\"text\":\"<value>\"` (each quote
-- backslash-escaped once). After we pull out that escaped value we
-- un-escape `\"`, `\\`, `\n`, `\t` to recover the human-readable string.
--
-- We avoid pulling in a full JSON parser because chat.lua's host VM
-- (nefor-tui) doesn't expose `nefor.json`, and the picker is dev
-- tooling — a regex-tier extraction is fine for a one-line preview.
local function extract_submit_text(line)
  -- Two scan modes, picked by which marker matches:
  --   * doubly-escaped:  `\"text\":\"` — payload is a JSON-encoded
  --                      string inside a JSON row (the production
  --                      shape; persist_envelope wraps the wire JSON
  --                      via json.encode of the row table).
  --   * singly-escaped:  `"text":"`    — the inner envelope written
  --                      directly without the row-wrapper layer
  --                      (test fixtures).
  local _, marker_end = line:find([[\"text\":\"]], 1, true)
  local doubly_encoded = marker_end ~= nil
  if marker_end == nil then
    _, marker_end = line:find('"text":"', 1, true)
    if marker_end == nil then return nil end
  end

  local i = marker_end + 1
  local out = {}
  local n = #line
  while i <= n do
    local c = line:sub(i, i)
    if c == "\\" and i + 1 <= n then
      local nxt = line:sub(i + 1, i + 1)
      if doubly_encoded then
        -- Doubly-encoded: every char of the inner JSON has each `"`
        -- written as `\"` and each `\` as `\\` in the file. The
        -- inner string's closing quote therefore appears as `\"`
        -- (2 chars). An escaped quote inside the inner string —
        -- which represents a literal `"` in the original text —
        -- appears as `\\\"` (3 chars), and a literal backslash as
        -- `\\\\` (4 chars).
        if nxt == '"' then
          return table.concat(out)
        elseif nxt == "\\" and i + 2 <= n then
          local nnxt = line:sub(i + 2, i + 2)
          if nnxt == '"' then
            out[#out + 1] = '"'
            i = i + 3
          elseif nnxt == "\\" then
            out[#out + 1] = "\\"
            i = i + 4
          elseif nnxt == "n" then
            out[#out + 1] = "\n"
            i = i + 3
          elseif nnxt == "t" then
            out[#out + 1] = "\t"
            i = i + 3
          else
            out[#out + 1] = nnxt
            i = i + 3
          end
        else
          out[#out + 1] = nxt
          i = i + 2
        end
      else
        -- Singly-encoded: standard JSON string escapes.
        if nxt == '"' then
          out[#out + 1] = '"'
          i = i + 2
        elseif nxt == "\\" then
          out[#out + 1] = "\\"
          i = i + 2
        elseif nxt == "n" then
          out[#out + 1] = "\n"
          i = i + 2
        elseif nxt == "t" then
          out[#out + 1] = "\t"
          i = i + 2
        else
          out[#out + 1] = nxt
          i = i + 2
        end
      end
    elseif c == '"' and not doubly_encoded then
      return table.concat(out)
    else
      out[#out + 1] = c
      i = i + 1
    end
  end
  return nil
end

-- Extract started_at from the JSONL header: a known-shape line
--   {"_session":true,"session_id":"...","parent_session":...,"started_at":"<iso>"}
-- Pattern-match the `"started_at":"<value>"` field; same rationale as
-- extract_submit_text — we don't need a full JSON parser.
local function extract_started_at(header_line)
  local v = header_line:match('"started_at"%s*:%s*"([^"]+)"')
  return v
end

-- List up to `limit` newest sessions on disk. Each row:
--   { id = "<uuid>", path = "<full>", started_at = "<iso>", preview = "<first user prompt>" }
-- The preview is best-effort: we scan the JSONL for the first
-- `chat.input.submit` event and pull `text`. Sessions with no submits
-- (e.g. crashed boots) get a "(no submits)" placeholder. `started_at`
-- comes from the header — falls back to "?" if the header is missing
-- or malformed.
local function list_recent_sessions(limit)
  local dir = session_dir()
  if dir == nil then return {} end
  -- `ls -t` sorts newest mtime first. Pure-Lua dir iteration would need
  -- LuaFileSystem; `io.popen` is enabled by mlua's safe stdlib.
  local cmd = string.format("ls -t %q 2>/dev/null", dir)
  local pipe = io.popen(cmd)
  if pipe == nil then return {} end
  local sessions = {}
  for fname in pipe:lines() do
    if #sessions >= limit then break end
    local id = fname:match("^([%w%-]+)%.jsonl$")
    if id ~= nil then
      sessions[#sessions + 1] = { id = id, path = dir .. "/" .. fname }
    end
  end
  pipe:close()
  -- Enrich each row with header timestamp + first prompt preview. Read
  -- line-by-line and stop at the first chat.input.submit hit so
  -- multi-megabyte sessions don't slurp the whole file.
  for _, s in ipairs(sessions) do
    local fh = io.open(s.path, "r")
    if fh ~= nil then
      local header_line = fh:read("*l") or ""
      s.started_at = extract_started_at(header_line) or "?"
      local preview = nil
      for line in fh:lines() do
        -- Cheap substring filter — most lines aren't chat.input.submit.
        if line:find("chat.input.submit", 1, true) ~= nil then
          preview = extract_submit_text(line)
          if preview ~= nil then break end
        end
      end
      fh:close()
      s.preview = preview or "(no submits)"
    else
      s.started_at = "?"
      s.preview    = "(unreadable)"
    end
  end
  return sessions
end

-- Truncate `text` to `n` columns (byte-count proxy; non-ASCII previews
-- may render slightly off but the picker is dev-tooling). Newlines
-- collapse to spaces so multi-line prompts render as a single row.
local function clip_preview(text, n)
  if text == nil then return "" end
  text = tostring(text):gsub("\n", " "):gsub("\r", " ")
  if #text <= n then return text end
  return text:sub(1, math.max(0, n - 1)) .. "…"
end

-- Format the started_at timestamp for picker rows. ISO 8601 → "MM-DD HH:MM".
-- Falls back to the raw string if it doesn't parse.
local function format_started_at(s)
  if type(s) ~= "string" then return "?" end
  local mo, dy, hh, mm = s:match("^%d%d%d%d%-(%d%d)%-(%d%d)T(%d%d):(%d%d)")
  if mo == nil then return s end
  return string.format("%s-%s %s:%s", mo, dy, hh, mm)
end

-- Build a `sessions.resume_request` send_to effect for `id`. The starter's
-- sessions module subscribes to this kind via `nefor.bus.on_event` and
-- runs the swap sequence in-process. No process exit, no file write —
-- the TUI stays alive across the resume.
local function emit_resume_request(id)
  return {
    kind   = "send_to",
    target = "engine",
    body   = { kind = "sessions.resume_request", session_id = id },
  }
end

-- Session picker popup. Border is HL_USER. Layout:
--   ╭── resume a session ──────╮
--   │ MM-DD HH:MM  <preview>   │  ← cursor row inverted
--   │ MM-DD HH:MM  <preview>   │
--   │ ↑/↓ Enter pick · Esc     │
--   ╰──────────────────────────╯
local function popup_session_picker(state)
  if not state.popup or state.popup.variant ~= "session_picker" then return nil end
  local p = state.popup
  local sessions = p.sessions or {}
  local cursor = p.cursor or 1
  if cursor < 1 then cursor = 1 end
  if cursor > #sessions and #sessions > 0 then cursor = #sessions end

  local body_rows = {}
  if #sessions == 0 then
    body_rows[#body_rows + 1] = tui.text {
      content = "No saved sessions found.",
      style   = STYLE.status_dim, wrap = "word",
    }
    body_rows[#body_rows + 1] = tui.text {
      content = "Sessions live at " .. (session_dir() or "<unknown>"),
      style   = STYLE.status_dim, wrap = "word",
    }
  else
    -- Window the visible rows around the cursor.
    local cap = 12
    local first = 1
    if #sessions > cap then
      first = math.max(1, math.min(cursor - cap + 1, #sessions - cap + 1))
      if first < 1 then first = 1 end
    end
    local last = math.min(first + cap - 1, #sessions)
    for i = first, last do
      local s = sessions[i]
      local stamp = format_started_at(s.started_at)
      local preview = clip_preview(s.preview, 50)
      local row = string.format("%-12s  %s", stamp, preview)
      local style = (i == cursor)
        and { fg = "#000000", bg = C.user }
        or  STYLE.status
      body_rows[#body_rows + 1] = tui.text {
        content = row, style = style, wrap = "none",
      }
    end
  end

  local children = {
    tui.text {
      content = "── resume a session ──",
      style   = STYLE.popup_user,
      wrap    = "none",
    },
    tui.column { gap = 0, children = body_rows },
    tui.text {
      content = "↑/↓ select · Enter resume · Esc cancel",
      style   = STYLE.status_dim,
      wrap    = "none",
    },
  }

  return tui.anchored {
    anchor = "center",
    width  = "70%",
    height = "60%",
    child  = bordered_popup(
      "popup_session_picker",
      tui.padding {
        value = 1,
        child = tui.column { gap = 1, children = children },
      },
      STYLE.popup_user
    ),
  }
end

-- Model picker popup (spec section 12). Border is HL_USER. Layout:
--   ╭── pick a model ──╮
--   │ search: <query>  │
--   │ ───────────────  │
--   │ provider  model  │  ← row, cursor row inverted
--   │ provider  model  │
--   │ loading from N   │  ← footer (when awaiting providers)
--   ╰──────────────────╯
local function popup_model_picker(state)
  if not state.popup or state.popup.variant ~= "model_picker" then return nil end
  local p = state.popup
  local matches = model_picker_filter(p.models, p.query)
  local cursor = p.cursor or 1
  if cursor < 1 then cursor = 1 end
  if cursor > #matches and #matches > 0 then cursor = #matches end
  -- Determine widest provider name for column alignment.
  local prov_w = 0
  for _, e in ipairs(matches) do
    if e.provider and #e.provider > prov_w then prov_w = #e.provider end
  end
  if prov_w > 20 then prov_w = 20 end

  local body_rows = {}
  if #matches == 0 then
    if awaiting_count(p.awaiting) == 0 and (p.models == nil or #p.models == 0) then
      body_rows[#body_rows + 1] = tui.text {
        content = "No providers connected.",
        style   = STYLE.status_dim, wrap = "word",
      }
      body_rows[#body_rows + 1] = tui.text {
        content = "Wire one up in init.lua (see docs/provider-plugins.md).",
        style   = STYLE.status_dim, wrap = "word",
      }
    else
      body_rows[#body_rows + 1] = tui.text {
        content = "(no matches)",
        style   = STYLE.status_dim, wrap = "none",
      }
    end
  else
    -- Window the visible rows around the cursor so a long list scrolls.
    local cap = 12
    local first = 1
    if #matches > cap then
      first = math.max(1, math.min(cursor - cap + 1, #matches - cap + 1))
      if first < 1 then first = 1 end
    end
    local last = math.min(first + cap - 1, #matches)
    for i = first, last do
      local e = matches[i]
      local row = string.format("%-" .. prov_w .. "s  %s", e.provider or "?", e.model or "?")
      local style = (i == cursor)
        and { fg = "#000000", bg = C.user }
        or  STYLE.status
      body_rows[#body_rows + 1] = tui.text {
        content = row, style = style, wrap = "none",
      }
    end
  end

  local awaiting_n = awaiting_count(p.awaiting)
  local children = {
    tui.text {
      content = "── pick a model ──",
      style   = STYLE.popup_user,
      wrap    = "none",
    },
    tui.text {
      content = "search: " .. (p.query or ""),
      style   = STYLE.status,
      wrap    = "none",
    },
    tui.text {
      content = string.rep("─", 40),
      style   = STYLE.footer,
      wrap    = "none",
    },
    tui.column { gap = 0, children = body_rows },
  }
  if awaiting_n > 0 then
    children[#children + 1] = tui.text {
      content = string.format("loading from %d provider(s)…", awaiting_n),
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  children[#children + 1] = tui.text {
    content = "↑/↓ select · Enter pick · Esc close · type to filter",
    style   = STYLE.status_dim,
    wrap    = "none",
  }

  return tui.anchored {
    anchor = "center",
    width  = "60%",
    height = "60%",
    child  = bordered_popup(
      "popup_model_picker",
      tui.padding {
        value = 1,
        child = tui.column { gap = 1, children = children },
      },
      STYLE.popup_user
    ),
  }
end

-- Toast = ephemeral, non-blocking (e.g., "copied N chars").
-- Popup = blocking, requires user input (esc) for dismissal.
-- Don't blur this distinction.
--
-- Inline toast: bordered pill anchored bottom-RIGHT of the body area
-- (overlaying the transcript only — never covers the input or
-- statusline). Auto-dismisses on `expires_at_ms`. The text leaf owns
-- its full allocated rect (engine paints trailing cells with the
-- text's own style), so the bg colour band reaches edge-to-edge — no
-- per-row main_column chars peek through.
--
-- Animation: translate-only horizontal slide. The pill keeps full
-- size throughout (no width clip, no height clip). On enter the pill
-- slides leftward from the right edge to its rest position over
-- `TOAST_ENTER_MS` with ease-out cubic; on exit it mirrors back over
-- `TOAST_EXIT_MS` with ease-in cubic. The render-loop keepalive (see
-- `render_keepalive`) ensures `tui.now_ms()` re-evaluates each frame
-- so the slide is smooth and the toast disappears at `expires_at_ms`
-- rather than freezing on the last shown state.
--
-- `tui.anchored`'s offset is clamped to keep the pill on-screen, so
-- "off-screen right" is approximated by the rest position sitting
-- `TOAST_REST_INSET` cells inward from the right edge — the slide is
-- visible across that inset distance. The translation is real (no
-- clipping), just bounded by the parent rect.
local TOAST_FG_INFO        = "#88ccff"
local TOAST_BORDER_INFO    = { fg = "#7faaaa" }   -- dim cyan rail to match blockquote palette
-- warn/error variants reserved; only info wired by current call sites.
-- Reference the popup warn/error palette so toast variants line up
-- with the rest of the chrome when later call sites adopt them.
local TOAST_FG_WARN        = C.status_warn
local TOAST_BORDER_WARN    = { fg = C.status_warn }
local TOAST_FG_ERROR       = C.status_danger
local TOAST_BORDER_ERROR   = { fg = C.status_danger }
local TOAST_FULL_HEIGHT    = 3    -- top rule + text + bottom rule (no inner padding)
local TOAST_ENTER_MS       = 220
local TOAST_EXIT_MS        = 220
-- Extra cells of dashes past the right edge of the window. The
-- anchored rect clamps to parent width, so these dashes get clipped
-- on the right — visually the top/bottom rules look like they
-- continue off-screen rather than terminating exactly at the edge.
local TOAST_RIGHT_OVERFLOW = 6

-- Easing helpers. Domain [0,1]; clamp before applying.
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

-- Resolve the (fg, border) palette for a toast level. Unknown levels
-- fall back to "info" so a malformed envelope doesn't crash render.
local function toast_palette(level)
  if level == "warn" then
    return TOAST_FG_WARN, TOAST_BORDER_WARN
  elseif level == "error" then
    return TOAST_FG_ERROR, TOAST_BORDER_ERROR
  else
    -- "info" or anything else → info palette. Current call sites only
    -- wire info; warn/error variants are reserved for future use.
    return TOAST_FG_INFO, TOAST_BORDER_INFO
  end
end

local function inline_toast(state)
  if not state.toast then return nil end
  local now     = tui.now_ms()
  local created = state.toast.created_at_ms or now
  local expires = state.toast.expires_at_ms or (created + 2000)
  if now >= expires then return nil end

  local elapsed   = now - created
  local time_left = expires - now

  local text = state.toast.text or ""
  local fg, border = toast_palette(state.toast.level)

  -- Small pill anchored bottom-right. Width at rest = `╭` corner +
  -- space + text + extra dashes that overflow past the right edge.
  -- The slide is implemented by varying the pill's WIDTH from 0
  -- (off-screen) to pill_w_at_rest. Because the pill is anchored at
  -- the right edge, the rect grows LEFTWARD as visible_w increases
  -- and the text widgets' content gets truncated on the right by the
  -- rect's width. The TOAST_RIGHT_OVERFLOW slack means the rules at
  -- rest extend past the visible right edge of the window — they
  -- look like they're going "off-screen" rather than stopping
  -- exactly at the edge.
  local pill_w_at_rest = 2 + #text + TOAST_RIGHT_OVERFLOW
  local total_slide    = pill_w_at_rest
  local distance_slid
  if elapsed < TOAST_ENTER_MS then
    local t = elapsed / TOAST_ENTER_MS
    distance_slid = total_slide * ease_out_cubic(t)
  elseif time_left < TOAST_EXIT_MS then
    local t = 1 - (time_left / TOAST_EXIT_MS)
    distance_slid = total_slide * (1 - ease_in_cubic(t))
  else
    distance_slid = total_slide
  end
  distance_slid = math.floor(distance_slid + 0.5)

  local visible_w = math.min(distance_slid, pill_w_at_rest)
  if visible_w <= 0 then return nil end

  -- At-rest content: 3 strings, each `pill_w_at_rest` cells wide.
  -- Chrome lives on the LEFT — `╭` / `│` / `╰` opens the pill on
  -- the leading side; the dashes extend rightward past the right
  -- edge of the window where the renderer clips them. The mid row
  -- pads with trailing spaces so the area below the dashes doesn't
  -- show the chrome from the layer underneath bleeding through.
  local top_rule    = "╭" .. string.rep("─", pill_w_at_rest - 1)
  local mid_text    = "│ " .. text .. string.rep(" ", TOAST_RIGHT_OVERFLOW)
  local bottom_rule = "╰" .. string.rep("─", pill_w_at_rest - 1)

  local body = tui.column {
    gap = 0,
    key = "toast-box",
    children = {
      tui.constrained {
        max_height = 1,
        child = tui.text { content = top_rule, style = border, wrap = "none" },
      },
      tui.constrained {
        max_height = 1,
        child = tui.text { content = mid_text, style = { fg = fg }, wrap = "none" },
      },
      tui.constrained {
        max_height = 1,
        child = tui.text { content = bottom_rule, style = border, wrap = "none" },
      },
    },
  }

  return tui.anchored {
    anchor   = "bottom-right",
    offset_x = 0,
    offset_y = 0,
    width    = visible_w,
    height   = TOAST_FULL_HEIGHT,
    child    = body,
  }
end

-- Old popup-style toast retained as a no-op shim so existing call
-- sites (compact { ..., popup_toast(state), ... }) keep compiling
-- while the inline_toast above takes over rendering. Anything left in
-- the popup stack would just stack on top of the inline one.
local function popup_toast(_state)
  return nil
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
  return string.format("%ds", math.floor(ms / 1000))
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
    child = tui.fill { char = "│", style = STYLE.dag_separator },
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
    -- 1-cell right padding so wrapped lines don't visually clash into
    -- the scrollbar's column.
    child     = tui.padding {
      value = { top = 0, right = 1, bottom = 0, left = 0 },
      child = tui.column { gap = 1, children = widgets },
    },
  }
end

-- Keep the engine's render loop alive at ~1Hz while any per-second
-- elapsed counter is on screen — `tui.now_ms()` only re-evaluates on a
-- render, and the engine renders only on state changes / animation
-- ticks. Without this, the DAG sidebar's "Ns" stalls between events
-- (the user sees stale numbers until something else re-renders, like
-- a scroll or keystroke). Mount only when something needs to refresh.
local KEEPALIVE_FRAMES = { "", "" }

local function any_dag_run_active(dag_runs)
  if type(dag_runs) ~= "table" then return false end
  for _, run in pairs(dag_runs) do
    if run.completed_at_ms == nil then return true end
  end
  return false
end

local function render_keepalive(state)
  -- Toast inclusion is load-bearing: without it the engine renders only
  -- on state changes, so the toast appears once and never re-renders to
  -- run its slide-out / disappearance. duration_ms = 100 keeps the
  -- toast slide animation smooth (~60fps engine tick when active);
  -- DAG-elapsed counters only need 1Hz but the extra ticks are free.
  if not (state.pending or any_dag_run_active(state.dag_runs) or state.toast) then
    return nil
  end
  return tui.animation {
    frames      = KEEPALIVE_FRAMES,
    duration_ms = 100,
  }
end

local function view(state)
  -- Input field with full-width rounded border per legacy spec section
  -- 7. The `tui.text_input` is the bare control; `bordered_box` wraps
  -- it in `╭─╮ │ ╰─╯` chrome so the input visually matches user
  -- message blocks. Border colour brightens (HL_USER) when the input
  -- is focused; dims to HL_STATUS_DIM when a popup steals focus.
  -- The input drops focus while certain popups own the keyboard.
  -- Tool permission expects single-char A/D; model picker takes
  -- printable chars as filter input — both paths require input to
  -- stop swallowing keys.
  local popup_owns_keys = state.popup and (
    state.popup.variant == "tool_permission" or
    state.popup.variant == "model_picker" or
    state.popup.variant == "session_picker"
  )
  local input_focused = state.focused_id == "input" and not popup_owns_keys
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
      -- No placeholder text: the bordered input below the transcript is
      -- self-explanatory, and a default hint just adds visual noise the
      -- user has to read past on every render. Slash-commands are
      -- discoverable via `/` autocomplete; help via `/help`.
    },
    input_border_style,
    -- Stable user-key on the bordered_box's outer column so the
    -- reconciler reuses the text_input instance (preserving cursor +
    -- selection + scroll) across renders that change main_column's
    -- child count — notably when the slash autocomplete dropdown
    -- appears or vanishes between the body row and the input.
    "input-field"
  )

  -- One-row blank spacer reused at the top of the chat column and
  -- the bottom (above the statusline). The sidebar gets no spacer:
  -- its vertical separator now runs full window height edge-to-edge.
  local function blank_row()
    return tui.constrained {
      max_height = 1,
      child = tui.fill { char = " " },
    }
  end

  -- Left column = chat surface. Top → bottom: 1-row top gap /
  -- transcript / slash autocomplete (when open) / input / statusline /
  -- 1-row bottom gap / keepalive. Statusline lives BELOW the input
  -- per legacy spec — pushing it above the input visibly inverts the
  -- screen weight, making the input feel like a status row rather
  -- than the primary focus surface. The bottom gap lifts the
  -- statusline off the very last row so it doesn't sit flush against
  -- the terminal frame.
  local left_column = tui.column {
    gap = 0,
    children = compact {
      blank_row(),
      tui.expanded { child = transcript(state) },
      slash_autocomplete_inline(state),
      input_field,
      statusline(state),
      blank_row(),
      -- Invisible 1Hz keepalive: forces re-render while DAG runs or the
      -- thinking ticker need the second-counter refreshed. Removed when
      -- nothing needs to tick.
      render_keepalive(state),
    },
  }

  -- Outer row: left column (chat) | separator | sidebar. No outer
  -- padding — the sidebar's vertical separator reaches the full
  -- window height (top and bottom edges flush), and per-element
  -- spacing is handled inside `left_column` and `dag_panel`.
  local main_row = tui.row {
    gap = 0,
    children = compact {
      tui.expanded { child = left_column },
      state.show_sidebar and vertical_separator() or nil,
      state.show_sidebar and dag_panel(state)        or nil,
    },
  }

  return tui.stack {
    children = compact {
      main_row,
      popup_help(state),
      popup_message(state),
      popup_model_picker(state),
      popup_session_picker(state),
      popup_tool_permission(state),
      -- Toast renders last so it sits above input, statusline, and
      -- every popup — non-blocking notifications must never be
      -- occluded by chrome below them.
      inline_toast(state),
      popup_toast(state),
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
    -- No in-flight entry. Two cases:
    --   1. Resume replay dropped the per-token deltas; this finalizer
    --      is the only event carrying the assistant text. Push a
    --      fully-formed entry from `final_text` so the message lands.
    --   2. Empty turn (e.g. error) — `final_text` is nil/empty;
    --      record only the durations.
    if final_text and #final_text > 0 then
      local entries = {}
      for i, v in ipairs(state.entries) do entries[i] = v end
      entries[#entries + 1] = {
        role        = "assistant",
        text        = final_text,
        kind        = "stream",
        streaming   = false,
        model       = model,
        duration_ms = duration_ms,
      }
      return shallow_merge(state, {
        entries          = entries,
        pending          = false,
        turn_started_at  = NIL_SENTINEL,
        last_turn_duration_ms = turn_dur,
      })
    end
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
    -- Any value mutation drops history navigation — once the user
    -- starts editing the recalled prompt it stops being a history slot
    -- and becomes the active draft. Clearing here keeps the next Up
    -- from jumping back to the navigation cursor mid-edit.
    state = shallow_merge(state, {
      input_value    = v,
      history_cursor = NIL_SENTINEL,
    })
    state = refresh_slash(state, v)
    return state, {}
  end

  if kind == "input.submit" then
    local text = msg.value or ""
    -- Slash autocomplete open + Enter → run the highlighted match,
    -- regardless of what fragment the user actually typed. Browser-style
    -- combobox semantics: pressing Enter while the dropdown is open
    -- selects the focused option, it doesn't submit the partial query.
    -- This lets `/mo` + Enter execute `/model` when the dropdown shows
    -- `/model` highlighted (matching legacy nefor's behaviour).
    if state.slash then
      local m = state.slash.matches and state.slash.matches[state.slash.cursor]
      if m then
        text = "/" .. m.name
      end
    end
    if #text == 0 then return state, {} end
    -- Slash dispatch.
    local cmd, args, _has_ws = parse_slash(text)
    if cmd == "quit" or cmd == "exit" then
      return state, { { kind = "exit" } }
    end
    if cmd == "new" or cmd == "clear" then
      -- `/new` mints a brand-new session on disk in addition to
      -- clearing the visual transcript. Without `sessions.new_request`,
      -- the on-disk session id stays put and every subsequent submit
      -- lands in the file the picker previewed before — so the picker
      -- only ever showed one growing entry no matter how many `/new`s
      -- the user typed. The starter's sessions module subscribes to
      -- this kind via `nefor.bus.on_event` and runs the in-process
      -- mint + swap (session_end → close+prune → open fresh →
      -- session_start → resume_done with replay=0). The
      -- `chat.interrupt_all` envelope is still emitted so any in-flight
      -- streaming aborts immediately rather than waiting for the
      -- session_end teardown to fan out via the broker.
      local cleared = shallow_merge(state, {
        entries = {}, in_flight = NIL_SENTINEL, input_value = "",
        pending = false, slash = NIL_SENTINEL,
        dag_runs = {},
        turn_started_at = NIL_SENTINEL,
        last_turn_duration_ms = NIL_SENTINEL,
        last_esc_ms = NIL_SENTINEL,
        history_cursor = NIL_SENTINEL,
        popup = NIL_SENTINEL,
      })
      return cleared, {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt_all" } },
        { kind = "send_to", target = "engine",
          body = { kind = "sessions.new_request" } },
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
      if args and #args > 0 then
        -- `/model <name>` — direct switch on the active provider.
        -- Active provider = first connected (alphabetical) when no
        -- explicit selection has been made yet.
        local provider = nil
        local connected = {}
        for n, st in pairs(state.auth or {}) do
          if st == "connected" then connected[#connected + 1] = n end
        end
        table.sort(connected)
        provider = connected[1]
        local body = { kind = "chat.model.set", model = args }
        if provider then body.provider = provider end
        return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine", body = body },
        }
      end
      -- `/model` (no args) — open the picker and fan out one
      -- `chat.model.list_requested` per connected provider so the
      -- popup can aggregate results as they land. Per legacy spec
      -- section 8 / 12. The adapter rejects requests that don't
      -- name a provider, so we MUST fan out per-provider here.
      local connected = {}
      for n, st in pairs(state.auth or {}) do
        if st == "connected" then connected[#connected + 1] = n end
      end
      table.sort(connected)
      local awaiting = {}
      for _, n in ipairs(connected) do awaiting[n] = true end
      local effects = {}
      for _, n in ipairs(connected) do
        effects[#effects + 1] = {
          kind = "send_to", target = "engine",
          body = { kind = "chat.model.list_requested", provider = n },
        }
      end
      return shallow_merge(state, {
        input_value = "", slash = NIL_SENTINEL,
        popup = {
          variant  = "model_picker",
          models   = {},
          query    = "",
          cursor   = 1,
          awaiting = awaiting,
        },
      }), effects
    end
    if cmd == "resume" then
      -- `/resume <session-id>` — direct: emit `sessions.resume_request`
      -- onto the bus and clear the input. The starter's sessions module
      -- runs the in-process swap (no exit, no sidechannel).
      if args and #args > 0 then
        local id = args:match("^([%w%-]+)") or args
        return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
          emit_resume_request(id),
        }
      end
      -- `/resume` (no args) — open the picker.
      local sessions = list_recent_sessions(10)
      return shallow_merge(state, {
        input_value = "", slash = NIL_SENTINEL,
        popup = {
          variant  = "session_picker",
          sessions = sessions,
          cursor   = 1,
        },
      }), {}
    end
    if cmd ~= nil then
      -- Unknown slash → generic chat.command for user-defined Lua handlers.
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.command", name = cmd, args = args or "" } },
      }
    end
    -- Plain text submit.
    --
    -- We push the user message LOCALLY for instant feedback, then
    -- emit `chat.input.submit`. The orchestrator's `for_chat` handler
    -- echoes the message back via `chat.message.append { role=user }`
    -- (so the message persists in the session log and replays on
    -- resume); the corresponding handler below dedupes that round-trip
    -- against `pending_user_echo` so we render once locally + once on
    -- replay, never twice live.
    local with_user = push_entry(state, { role = "user", text = text, kind = "text" })
    -- Prepend to prompt_history (newest at index 1) and cap. History
    -- recall reads from index 1, so prepending keeps the cursor model
    -- simple — Up = older = larger index, Down = newer = smaller.
    local history = { text }
    for i, v in ipairs(state.prompt_history or {}) do
      if i >= HISTORY_CAP then break end
      history[#history + 1] = v
    end
    local cleared = shallow_merge(with_user, {
      input_value = "", pending = true,
      turn_started_at = tui.now_ms(), slash = NIL_SENTINEL,
      prompt_history = history,
      history_cursor = NIL_SENTINEL,
      -- Mark the next bus-delivered chat.message.append with this
      -- exact text + role as the orchestrator's persist-echo and
      -- swallow it. Cleared after one match — sequential identical
      -- submits each set their own marker on submit, so the second
      -- echo doesn't get eaten by the first marker.
      pending_user_echo = text,
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
    -- 3) cancel prompt-history navigation (clear recalled value)
    if state.history_cursor ~= nil then
      return shallow_merge(state, {
        input_value    = "",
        history_cursor = NIL_SENTINEL,
      }), {}
    end
    -- 4) double-ESC escalation
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

  -- Tool permission popup keys. Routes to tool-gate via broadcast event
  -- (target hint is documentation-only; tool-gate matches by `id`).
  -- A / Enter → approve, D → deny. Esc handled in the popup-close branch
  -- above — also denies. The footer chrome advertises the same.
  if state.popup and state.popup.variant == "tool_permission" then
    if kind == "key.a" or kind == "key.A" or kind == "key.enter" then
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

  -- Model picker popup keys (legacy spec section 12). Up/Down move the
  -- cursor through the filtered list; Enter emits chat.model.set for
  -- the cursor row + closes; Backspace and printable chars edit the
  -- filter query (re-clamping cursor against the new filtered count).
  if state.popup and state.popup.variant == "model_picker" then
    local p = state.popup
    local filtered = model_picker_filter(p.models, p.query)
    if kind == "key.up" or kind == "key.down" then
      if #filtered == 0 then return state, {} end
      local cur = p.cursor or 1
      cur = (kind == "key.up") and (cur - 1) or (cur + 1)
      if cur < 1 then cur = #filtered end
      if cur > #filtered then cur = 1 end
      return shallow_merge(state, {
        popup = shallow_merge(p, { cursor = cur }),
      }), {}
    end
    if kind == "key.enter" then
      if #filtered == 0 then return state, {} end
      local sel = filtered[p.cursor or 1] or filtered[1]
      if not sel then return state, {} end
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = {
            kind     = "chat.model.set",
            provider = sel.provider,
            model    = sel.model,
          } },
      }
    end
    if kind == "key.backspace" then
      local q = p.query or ""
      if #q > 0 then q = q:sub(1, #q - 1) end
      return shallow_merge(state, {
        popup = shallow_merge(p, { query = q, cursor = 1 }),
      }), {}
    end
    -- Printable single-char filter input. The engine surfaces these as
    -- `key.<ch>` events when the input field has dropped focus (which
    -- it has — see `popup_owns_keys` above). `key.space` is a special
    -- name we have to map back to a literal space.
    if kind == "key.space" then
      return shallow_merge(state, {
        popup = shallow_merge(p, {
          query  = (p.query or "") .. " ",
          cursor = 1,
        }),
      }), {}
    end
    if kind:sub(1, 4) == "key." and #kind == 5 then
      local ch = kind:sub(5, 5)
      -- Filter pure printable ASCII characters into the query.
      local b = string.byte(ch)
      if b and b >= 33 and b <= 126 then
        return shallow_merge(state, {
          popup = shallow_merge(p, {
            query  = (p.query or "") .. ch,
            cursor = 1,
          }),
        }), {}
      end
    end
  end

  -- Session picker popup keys. Up/Down move cursor; Enter emits a
  -- `sessions.resume_request` envelope onto the NCP bus and dismisses
  -- the popup. The starter's sessions module subscribes via
  -- `nefor.bus.on_event` and runs the in-process swap. Esc handled in
  -- the popup-close branch above (closes without emitting). No filter
  -- input — the picker is small (≤10 sessions) and the timestamps +
  -- previews are scannable as-is.
  if state.popup and state.popup.variant == "session_picker" then
    local p = state.popup
    local sessions = p.sessions or {}
    if kind == "key.up" or kind == "key.down" then
      if #sessions == 0 then return state, {} end
      local cur = p.cursor or 1
      cur = (kind == "key.up") and (cur - 1) or (cur + 1)
      if cur < 1 then cur = #sessions end
      if cur > #sessions then cur = 1 end
      return shallow_merge(state, {
        popup = shallow_merge(p, { cursor = cur }),
      }), {}
    end
    if kind == "key.enter" then
      if #sessions == 0 then return state, {} end
      local sel = sessions[p.cursor or 1] or sessions[1]
      if not sel or not sel.id then return state, {} end
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        emit_resume_request(sel.id),
      }
    end
    -- All other keys swallow so they don't bubble into the input field.
    return state, {}
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
      if v == "model_picker" then return "popup_model_picker" end
      if v == "session_picker" then return "popup_session_picker" end
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
  --
  -- When NO popup owns scroll AND the input is empty (or the user is
  -- already navigating prompt history), Up/Down recall earlier prompts
  -- per legacy spec section 7. Up walks to older prompts; Down walks
  -- back to newer; reaching the latest+1 clears the buffer and ends
  -- navigation. Any non-arrow key (handled below in input.changed) drops
  -- history_cursor, so the next Up starts fresh.
  if kind == "key.up" then
    local target = active_scroll_key()
    if not target then
      local navigating = state.history_cursor ~= nil
      local empty = (state.input_value or "") == ""
      if (navigating or empty) and #(state.prompt_history or {}) > 0 then
        local cur = state.history_cursor or 0
        local n = #state.prompt_history
        local nxt = math.min(cur + 1, n)
        return shallow_merge(state, {
          input_value    = state.prompt_history[nxt],
          history_cursor = nxt,
        }), {}
      end
      tui.scroll_by("transcript", -1)
    else
      tui.scroll_by(target, -1)
    end
    return state, {}
  end
  if kind == "key.down" then
    local target = active_scroll_key()
    if not target then
      if state.history_cursor ~= nil then
        local cur = state.history_cursor
        if cur <= 1 then
          -- Stepping past the newest entry clears the input and ends
          -- history navigation.
          return shallow_merge(state, {
            input_value    = "",
            history_cursor = NIL_SENTINEL,
          }), {}
        end
        local nxt = cur - 1
        return shallow_merge(state, {
          input_value    = state.prompt_history[nxt],
          history_cursor = nxt,
        }), {}
      end
      tui.scroll_by("transcript", 1)
    else
      tui.scroll_by(target, 1)
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

  -- ── session lifecycle ───────────────────────────────────────────────
  -- The starter's `sessions` module emits four control events on the
  -- bus. `session_end` and `session_start` bracket a resume swap;
  -- `resume_done` is the "we're back, finalise rendering" signal.
  -- During replay (between session_start and resume_done) we paint
  -- envelopes normally — chat.message.append, chat.stream.* land in
  -- transcript exactly the way they would on a live turn. The resume
  -- envelopes ARE the past, so rendering them rebuilds the prior view.
  if kind == "sessions.session_end" then
    -- Tear down transcript + in-flight stream + popup. Keep the UI
    -- shell (model id, stats, sidebar toggle, etc.) — those rebuild
    -- from replayed events on resume_done. We also clear `pending`
    -- and `turn_started_at` so the statusline doesn't show a phantom
    -- in-flight turn from the outgoing session. `pending_user_echo`
    -- is dropped so a stranded marker (last submit's echo never
    -- arrived before resume kicked in) can't swallow the first
    -- replayed user message.
    return shallow_merge(state, {
      entries          = {},
      in_flight        = NIL_SENTINEL,
      pending          = false,
      turn_started_at  = NIL_SENTINEL,
      last_turn_duration_ms = NIL_SENTINEL,
      popup            = NIL_SENTINEL,
      toast            = NIL_SENTINEL,
      slash            = NIL_SENTINEL,
      dag_runs         = {},
      pending_user_echo = NIL_SENTINEL,
    }), {}
  end

  if kind == "sessions.session_start" then
    -- No-op. The /resume + /new paths already clear via session_end's
    -- handler above; at boot, state is already empty. The handler
    -- USED to defensively wipe `entries = {}` here, but that turned
    -- out to be the cause of a real bug: ncp.lua's replay-on-attach
    -- can deliver the boot session_start AFTER the user has typed
    -- their first prompt (the local-push went into entries first).
    -- Wiping then nukes the user's message; the orchestrator's
    -- chat.message.append echo arrives later and gets deduped against
    -- the pending_user_echo marker, so nothing ever repaints the user
    -- line — the transcript shows only the assistant's reply.
    return state, {}
  end

  if kind == "sessions.resume_done" then
    -- Live again. The transcript already reflects the replayed past;
    -- nothing to do beyond letting the next render fire (which it
    -- will from the surrounding update loop).
    return state, {}
  end

  if kind == "chat.reset" then
    -- agentic_workflow's `teardown_for_session_end` broadcasts
    -- chat.reset so the provider's chat-history map clears. The TUI
    -- receives it too (broadcast doesn't filter peers), but the
    -- transcript clear that USED to live here is redundant —
    -- sessions.session_end fires alongside chat.reset and already
    -- wipes entries. Pinning a no-op handler instead of letting the
    -- envelope fall through is intentional: it documents that the
    -- TUI deliberately ignores chat.reset, so a future contributor
    -- who's tempted to "do something on reset" lands here first and
    -- sees the comment explaining why session_end owns the clear.
    return state, {}
  end

  -- ── inbound chat-contract events ────────────────────────────────────
  if kind == "chat.message.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    -- Round-trip echo from the orchestrator's `for_chat` handler:
    -- when the user submits, we push locally for instant feedback
    -- AND emit `chat.input.submit` to the bus. The orchestrator
    -- replies with `chat.message.append { role=user, text=<same> }`
    -- so the user message lands in the session log (and replays).
    -- Live, that round-trip would render the same line twice; eat
    -- it once when the marker matches.
    --
    -- BUT: only swallow the echo if the local push actually landed in
    -- `state.entries`. The marker by itself isn't enough — if a
    -- session-lifecycle event wiped entries between the local push
    -- and this echo, eating the echo would leave the transcript with
    -- NO user line (the user submitted, the orchestrator started
    -- doing things, and the prompt visually disappeared). Only the
    -- orchestrator's echo can re-paint the user line in that case,
    -- so let it through. The check is "the tail of entries is a
    -- user-role entry with matching text" — that's exactly the
    -- shape the local push leaves.
    local role = msg.role or "system"
    if role == "user"
       and state.pending_user_echo ~= nil
       and state.pending_user_echo == text then
      local entries = state.entries or {}
      local tail = entries[#entries]
      local local_push_landed = tail
        and tail.role == "user"
        and tail.text == text
      if local_push_landed then
        return shallow_merge(state, { pending_user_echo = NIL_SENTINEL }), {}
      end
      -- Marker stranded by an intervening clear — fall through and
      -- push the echo so the user line is still visible. Clear the
      -- marker so a future genuine duplicate can't ride this branch.
      return push_entry(
        shallow_merge(state, { pending_user_echo = NIL_SENTINEL }),
        { role = role, text = text, kind = "text" }
      ), {}
    end
    return push_entry(state, {
      role = role, text = text, kind = "text",
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
    -- `level` ∈ "info" | "warn" | "error". Defaults to "info" so the
    -- existing call sites (which never set the field) keep their
    -- current cyan styling. warn / error variants reserved for future
    -- use; the render pipeline accepts them, no caller wires them yet.
    return shallow_merge(state, {
      toast = {
        text = msg.text or "",
        level = msg.level or "info",
        created_at_ms = now,
        expires_at_ms = now + ttl,
      },
    }), {}
  end

  if kind == "chat.model.set_ack" then
    return shallow_merge(state, {
      model = msg.model or state.model,
      max_tokens = model_max_tokens(msg.model) or state.max_tokens,
    }), {}
  end

  if kind == "chat.models.listed" then
    -- A provider answered the list-request. Append into the open
    -- model_picker popup if one is up; otherwise drop (legacy spec).
    if not (state.popup and state.popup.variant == "model_picker") then
      return state, {}
    end
    local provider = msg.provider or ""
    local list = msg.models or {}
    local prev = state.popup.models or {}
    -- Append new (provider, model) pairs, dedup, then sort.
    local seen = {}
    for _, e in ipairs(prev) do
      seen[(e.provider or "") .. "\0" .. (e.model or "")] = true
    end
    local merged = {}
    for _, e in ipairs(prev) do merged[#merged + 1] = e end
    if type(list) == "table" then
      for _, m in ipairs(list) do
        local key = provider .. "\0" .. tostring(m)
        if not seen[key] then
          merged[#merged + 1] = { provider = provider, model = tostring(m) }
          seen[key] = true
        end
      end
    end
    table.sort(merged, function(a, b)
      if a.provider == b.provider then return a.model < b.model end
      return a.provider < b.provider
    end)
    -- Drop the answering provider from the awaiting set.
    local prev_awaiting = state.popup.awaiting or {}
    local new_awaiting = {}
    for k, v in pairs(prev_awaiting) do new_awaiting[k] = v end
    new_awaiting[provider] = nil
    return shallow_merge(state, {
      popup = shallow_merge(state.popup, {
        models   = merged,
        awaiting = new_awaiting,
      }),
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

  if kind == "chat.tool.permission_request" then
    -- Wire shape from tool-gate (plugins/tool-gate/src/main.rs):
    --   { kind = "chat.tool.permission_request",
    --     id   = "<provider outer id>",
    --     tool = "<tool name>",
    --     args = <JSON object> }
    -- We render `args` into a small key/value summary. The response goes
    -- back as `tool.permission_response { id, decision }` (handled in the
    -- popup keymap below). `msg.input_pretty` and `msg.name` are kept as
    -- forward-compatible fallbacks in case future emitters pre-format.
    local args = msg.args
    local body
    if msg.input_pretty ~= nil then
      body = tostring(msg.input_pretty)
    elseif type(args) == "table" then
      body = format_args(args)
    elseif args ~= nil then
      body = tostring(args)
    else
      body = ""
    end
    return shallow_merge(state, {
      popup = {
        variant = "tool_permission",
        tool    = msg.tool or msg.name or "?",
        id      = msg.id,
        body    = body,
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

  -- Mouse drag-to-select: the engine extracts the highlighted text from
  -- the framebuffer and dispatches `mouse.selection` here. Policy lives
  -- in this file (chat.lua), not the engine — we copy non-empty
  -- selections to the clipboard and surface a short toast acknowledging
  -- the action. The engine's role ends at "here is the text"; what
  -- happens next is the surface's call.
  if kind == "mouse.selection" then
    local text = msg.text or ""
    if #text > 0 then
      -- Read `now` BEFORE calling `tui.copy_to_clipboard`. The clipboard
      -- binding hits a system-wide pasteboard (NSPasteboard on macOS,
      -- X/Wayland selections on Linux), which can block tens to hundreds
      -- of ms under contention. `tui.now_ms` is the cached frame clock
      -- the engine installs at the start of each dispatch — same value
      -- regardless of read order — but reading it up-front signals
      -- intent and survives a future refactor where the binding
      -- refreshes the clock mid-dispatch.
      local now = tui.now_ms()
      tui.copy_to_clipboard(text)
      return shallow_merge(state, {
        toast = {
          text = string.format("copied %d chars", #text),
          created_at_ms = now,
          -- 4 s lifetime: covers the 2 s default plus headroom for the
          -- clipboard call's wall-clock cost. Without this padding the
          -- toast can wink out before the user's eye registers it on
          -- slow / contended systems (and tests racing the same path
          -- flake intermittently).
          expires_at_ms = now + 4000,
        },
      }), {}
    end
    return state, {}
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
