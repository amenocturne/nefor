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
--   graph.run_started, graph.node.fired, tool.result.
--
-- Outbound:
--   chat.input.submit, chat.interrupt, chat.interrupt_all, chat.reset,
--   chat.command, tool.permission_response.

-- The nefor-tui binary loads this file via `--script chat.lua`. Its
-- embedded Lua VM starts with a vanilla package.path; locate the plugin
-- lib's `lua/` dir and install a searcher that resolves `nefor-tui`
-- and `nefor-tui.<sub>` requires against it. A custom searcher avoids
-- the filesystem mutation a path graft would need (the lib's init.lua
-- sits directly at `<lua-dir>/init.lua` rather than `<lua-dir>/<name>/`).
do
  local function path_exists(p)
    local f = io.open(p, "r")
    if f == nil then return false end
    f:close()
    return true
  end
  local candidates = {}
  local explicit = os.getenv("NEFOR_TUI_LUA_DIR")
  if explicit and explicit ~= "" then
    candidates[#candidates + 1] = explicit
  end
  local config_dir = os.getenv("NEFOR_CONFIG_DIR")
  if config_dir and config_dir ~= "" then
    candidates[#candidates + 1] = config_dir .. "/../plugins/nefor-tui/lua"
  end
  candidates[#candidates + 1] = "./plugins/nefor-tui/lua"
  candidates[#candidates + 1] = "../plugins/nefor-tui/lua"

  local lua_dir
  for _, c in ipairs(candidates) do
    if path_exists(c .. "/init.lua") then
      lua_dir = c
      break
    end
  end
  if lua_dir == nil then
    error("starter/chat.lua: could not locate plugins/nefor-tui/lua/init.lua")
  end

  local PREFIX = "nefor-tui"
  local searchers = package.searchers or package.loaders
  table.insert(searchers, 1, function(name)
    if name ~= PREFIX and name:sub(1, #PREFIX + 1) ~= PREFIX .. "." then
      return nil
    end
    local rel
    if name == PREFIX then
      rel = "/init.lua"
    else
      -- nefor-tui.widget.popup -> /widget/popup.lua (with init.lua fallback)
      local sub = name:sub(#PREFIX + 2):gsub("%.", "/")
      rel = "/" .. sub .. ".lua"
    end
    local file_path = lua_dir .. rel
    if not path_exists(file_path) then
      local init_path = lua_dir .. rel:gsub("%.lua$", "/init.lua")
      if path_exists(init_path) then file_path = init_path
      else return "\n\tno file " .. file_path end
    end
    local chunk, err = loadfile(file_path)
    if chunk == nil then return "\n\t" .. tostring(err) end
    return chunk, file_path
  end)
end

local tui_lib       = require("nefor-tui")
local W             = tui_lib.widget
local _util         = tui_lib.util
local bordered_box  = _util.bordered_box
local shallow_merge = _util.shallow_merge
local NIL_SENTINEL  = _util.NIL

-- Drop nil holes from an array so reducer-built children lists stay
-- dense. Lua's table constructor with conditionally-nil entries leaves
-- gaps that break `ipairs`; the renderer relies on contiguous indices.
local function compact(list)
  local out = {}
  if list == nil then return out end
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
  -- Code-block bg: a clearly-grey rectangle, not the near-black
  -- #202020 the legacy spec carried — at low display contrast / on
  -- terminals with a warm color profile the old value rendered as a
  -- muddy brownish patch behind the glyphs (Bug A3 colour half).
  -- #3a3a3a is light enough to read as 'grey rectangle' on every
  -- profile while still sitting behind the C0C0C0 fg with comfortable
  -- legibility (~7:1 contrast).
  md_code_inline_bg = "#3a3a3a",
  md_code_block_bg  = "#3a3a3a",
  footer          = "#707070",  -- HL_FOOTER
  -- Plan-message border. Yellow distinguishes a write-review plan from
  -- the user's blue block and the system's grey-italic line. Bright
  -- gold reads on every terminal profile and doesn't collide with the
  -- existing `status_warn` (#D7AF5F) which carries semantic "warning"
  -- weight; plans aren't warnings, they're a third entry kind.
  plan            = "#FFD75F",
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
  -- Plan-entry chrome (yellow border). Same `bold = true` weight the
  -- user_chrome carries so the two read as parallel "input/output of a
  -- decision" frames at equal visual weight.
  plan_chrome           = { fg = C.plan, bold = true },
  -- Approved plans dim the border so the chat scroll de-emphasises
  -- already-resolved plans without actually hiding them. Rejected
  -- plans go danger-red so they stand out as "do NOT proceed on this".
  plan_chrome_approved  = { fg = C.plan, italic = true },
  plan_chrome_rejected  = { fg = C.status_danger, strikethrough = true },
  plan_subtitle         = { fg = C.footer },
  plan_hint             = { fg = C.footer, italic = true },
  plan_status_approved  = { fg = C.status_ok, bold = true },
  plan_status_rejected  = { fg = C.status_danger, bold = true },
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
--                       { kind = "plan", plan_id, text, submitted_at,
--                         status = "pending" | "approved" | "rejected" }
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
--   at_complete       nil | { matches, cursor, token, base_dir, leaf }
--   last_esc_ms       ms of the most recent ESC press (for double-ESC)
--   dag_runs          map keyed by run_id
--   prompt_history    list of submitted prompts (newest at index 1, cap 200)
--   history_cursor    nil = not navigating; integer = index into prompt_history

local DAG_LINGER_MS  = 2000
local DOUBLE_ESC_MS  = 600

-- Shell-style input-history cap. The submitted prompts (NOT every
-- keystroke — the user-typed text at submit time) are kept in-memory
-- under `state.prompt_history` AND mirrored to a single jsonl-ish file
-- on disk so they survive nefor restarts; arrow-up in the chat input
-- recalls them in reverse order. Past the cap the oldest entries roll
-- off the disk file the next time we trim. One constant drives both
-- the in-memory cap (issue #39) and the on-disk trim.
local INPUT_HISTORY_MAX = 50

------------------------------------------------------------------------
-- shell-style input history (issue #39)
------------------------------------------------------------------------
-- Single shared file at `<data_root>/input-history` (NOT per-session —
-- history is the user's, not the chat's). One submitted prompt per
-- line, escaped so multi-line pastes round-trip without breaking the
-- per-line frame. The chat-input arrow-up reducer already navigates an
-- in-memory `state.prompt_history`; this layer just keeps that list
-- mirrored to disk so it survives a nefor restart.
--
-- Defined up here, near the constant + initial_state, so both
-- `initial_state` (which hydrates from disk on first run) and the
-- submit reducer (which appends after every Enter) can call it as a
-- file-level local. Forward-referencing a `local function` declared
-- later in the chunk would resolve through globals at call time
-- (Lua's lexical-scope rules), so the file order matters.

-- Same env-var precedence as `sessions.lua`'s `compute_data_root` so
-- input-history sits next to session jsonls under the same root.
-- Inlined here rather than reusing `nefor_data_root` further down the
-- file because that helper is declared after `initial_state`, and Lua
-- closures bind locals at declaration time — `initial_state` would
-- otherwise see `nefor_data_root` as a missing global.
local function input_history_data_root()
  local override = os.getenv("NEFOR_DATA_HOME")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function input_history_path()
  local root = input_history_data_root()
  if root == nil then return nil end
  return root .. "/input-history"
end

-- One-line escaping: `\` → `\\`, real `\n` → `\n` literal (two chars),
-- real `\r` → `\r`. Decode reverses. Together this guarantees every
-- entry fits on a single physical line in the file regardless of
-- newlines / tabs / backslashes the user pasted in. Cheaper than
-- pulling in a JSON parser (the TUI Lua VM doesn't expose
-- `nefor.json`) and the format is self-describing — a developer
-- reading the file sees the obvious shape.
local function encode_history_line(text)
  if text == nil then return "" end
  local s = tostring(text)
  s = s:gsub("\\", "\\\\")
  s = s:gsub("\n", "\\n")
  s = s:gsub("\r", "\\r")
  return s
end

local function decode_history_line(line)
  if line == nil then return nil end
  local out = {}
  local i = 1
  local n = #line
  while i <= n do
    local c = line:sub(i, i)
    if c == "\\" and i < n then
      local nxt = line:sub(i + 1, i + 1)
      if nxt == "n" then
        out[#out + 1] = "\n"
        i = i + 2
      elseif nxt == "r" then
        out[#out + 1] = "\r"
        i = i + 2
      elseif nxt == "\\" then
        out[#out + 1] = "\\"
        i = i + 2
      else
        -- Unknown escape — keep verbatim. Future-proof against new
        -- escape kinds added by readers without breaking existing
        -- files.
        out[#out + 1] = c
        i = i + 1
      end
    else
      out[#out + 1] = c
      i = i + 1
    end
  end
  return table.concat(out)
end

-- Best-effort `mkdir -p` so the writer can drop the file even on a
-- fresh data-root. Lua doesn't ship an in-process mkdir; shell out the
-- same way `sessions.lua`'s `ensure_dir` does. Errors here mean the
-- writer's `io.open` will fail next, which the writer logs and
-- swallows — history just won't persist this session.
local function ensure_history_dir()
  local root = input_history_data_root()
  if root == nil then return end
  os.execute(string.format("mkdir -p %q 2>/dev/null", root))
end

-- Read the on-disk history into the in-memory list shape the rest of
-- chat.lua uses: newest at index 1. The file is written newest-first
-- on every append, so a forward read into a list keeps that order.
-- Caps at INPUT_HISTORY_MAX defensively in case an older nefor wrote
-- beyond the current cap.
local function load_input_history()
  local path = input_history_path()
  if path == nil then return {} end
  local f = io.open(path, "r")
  if f == nil then return {} end
  local out = {}
  for line in f:lines() do
    if #line > 0 then
      out[#out + 1] = decode_history_line(line)
      if #out >= INPUT_HISTORY_MAX then break end
    end
  end
  f:close()
  return out
end

-- Persist a `history` list to the input-history file. Called after
-- every submit; rewrites the whole file rather than appending +
-- truncating because the file is small (≤ INPUT_HISTORY_MAX lines)
-- and the rewrite is atomic enough for our durability needs (last
-- session crash loses at most the tail entry — `os.rename`-style
-- atomic-replace via tmp file is overkill for shell-history-grade
-- data). I/O failure is best-effort: log + continue so a read-only
-- data dir doesn't poison submission.
local function persist_input_history(history)
  local path = input_history_path()
  if path == nil then return end
  ensure_history_dir()
  local f = io.open(path, "w")
  if f == nil then return end
  for i = 1, math.min(#history, INPUT_HISTORY_MAX) do
    f:write(encode_history_line(history[i]))
    f:write("\n")
  end
  f:close()
end

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
    at_complete      = nil,
    last_esc_ms      = nil,
    dag_runs         = {},
    toast            = nil,  -- { text, expires_at_ms }
    -- Hydrate from <data_root>/input-history so arrow-up in the chat
    -- input recalls submissions from prior nefor processes (issue #39
    -- — shell-style persistent history). Empty on first run / read
    -- failure.
    prompt_history   = load_input_history(),
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
-- @path autocomplete (#XX)
------------------------------------------------------------------------
-- Mirrors slash autocomplete shape (matches/cursor/render) but the
-- completion source is the filesystem under CWD, walked one directory
-- level at a time to match bash-tab-completion intuition: `@src/m`
-- shows files in `src/` whose name starts with `m`, NOT a recursive
-- walk from CWD.
--
-- Trigger: the input's *last whitespace-separated word* starts with
-- `@`. This matches expand_at_path_refs's tokenisation (`@<non-ws>`)
-- and lets the user type an `@`-ref anywhere in a sentence, not just
-- as the first character (`/` is fixed at column 0; `@` floats).
--
-- Excluded entries: hidden files (leading `.`), `.git`, `node_modules`,
-- `target`, `__pycache__`. Conservative allowlist; not a full
-- gitignore parser per spec.

local AT_COMPLETION_CAP = 200

local AT_COMPLETION_IGNORE = {
  [".git"] = true, ["node_modules"] = true,
  ["target"] = true, ["__pycache__"] = true,
}

-- Find the active `@token` in `text`. Returns `nil` if there isn't
-- one. The active token is the trailing `@<non-ws>*` substring at end
-- of input — completion only fires while the cursor is conceptually
-- on the token being typed. We approximate "cursor at end" with
-- "value ends with the token" because text_input doesn't surface its
-- per-instance cursor through the `input.changed` payload (the value
-- is what we have; for the QoL win this is enough — same fidelity as
-- expand_at_path_refs which also only sees the submitted value).
local function active_at_token(text)
  if text == nil or text == "" then return nil end
  -- Find last `@` that's at start-of-string or preceded by whitespace.
  local at_pos = nil
  for i = #text, 1, -1 do
    local c = text:sub(i, i)
    if c == "@" then
      if i == 1 or text:sub(i - 1, i - 1):match("%s") then
        at_pos = i
      end
      break
    end
    if c:match("%s") then break end
  end
  if at_pos == nil then return nil end
  local token_body = text:sub(at_pos + 1)
  -- Token must contain no whitespace — completion stops once the user
  -- types past the token (a space after `@foo`).
  if token_body:find("%s") ~= nil then return nil end
  return text:sub(at_pos), at_pos, token_body
end

-- Split `body` (the part after `@`) into (base_dir, leaf). `base_dir`
-- is everything up to and including the last `/`; `leaf` is the
-- (case-insensitive prefix-matched) filter against directory entries.
-- Trailing `/` means base_dir = body, leaf = "".
local function split_at_body(body)
  local last_slash = body:find("/[^/]*$")
  if last_slash == nil then
    return "", body
  end
  return body:sub(1, last_slash), body:sub(last_slash + 1)
end

-- Resolve `base_dir` against CWD. Empty → CWD. Paths starting with
-- `/` are treated as absolute. Trailing slash kept (ls handles it).
local function resolve_base_dir(base_dir)
  if base_dir == nil or base_dir == "" then return "." end
  if base_dir:sub(1, 1) == "/" then return base_dir end
  return "./" .. base_dir
end

-- List entries under `dir`. Returns list of `{ name, is_dir }`.
-- Errors (missing dir, permission denied) yield empty list so a
-- half-typed dir name silently produces "no matches" rather than a
-- Lua error.
--
-- Backed by `nefor.fs.list_dir`, a Rust readdir bridged into Lua
-- (plugins/nefor-tui/src/fs.rs). The previous implementation shelled
-- out via `io.popen("ls -1Ap …")`, which produced `shell-init: cwd
-- not found` warnings when parallel tests inherited a since-deleted
-- tempdir cwd and inflated parallel-mode wall-time from ~90ms to
-- >60s due to fork contention. The Rust path is a single readdir
-- with no subprocess.
--
-- Filtering: skip dotfiles (leading `.`) and the
-- AT_COMPLETION_IGNORE allowlist; cap at AT_COMPLETION_CAP entries.
-- Sort dirs-first then case-insensitive alphabetical so drill-down
-- candidates lead the popup. The cache in `build_at_complete` reuses
-- the listing across keystrokes within the same dir.
local function ls_entries(dir)
  local entries, err = nefor.fs.list_dir(dir)
  if entries == nil then
    -- Half-typed dir, permission denied, etc. Silently return empty
    -- so the popup shows "no matches" rather than raising. The error
    -- string is intentionally not surfaced — the user's signal is the
    -- empty popup, not a Lua-level error message.
    local _ = err
    return {}
  end
  local out = {}
  for _, e in ipairs(entries) do
    local name = e.name
    if name:sub(1, 1) ~= "." and not AT_COMPLETION_IGNORE[name] then
      out[#out + 1] = { name = name, is_dir = e.is_dir }
      if #out >= AT_COMPLETION_CAP then break end
    end
  end
  table.sort(out, function(a, b)
    -- Directories first, then alphabetical case-insensitive — drives
    -- the user toward drill-down candidates over peer-level files.
    if a.is_dir ~= b.is_dir then return a.is_dir end
    return a.name:lower() < b.name:lower()
  end)
  return out
end

local function at_filter(entries, leaf)
  local q = (leaf or ""):lower()
  if q == "" then return entries end
  local out = {}
  for _, e in ipairs(entries) do
    if e.name:lower():sub(1, #q) == q then out[#out + 1] = e end
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
    -- Label the trailing block by terminal status. `error:` (red) for
    -- the deny / policy / unknown-tool / timeout paths so the block
    -- reads as a denial rather than an empty `output:` (Bug B); the
    -- tool-gate wrapper now puts the error message into the `output`
    -- field so it lands here instead of being dropped on the floor.
    local label, label_style
    if entry.error then
      label, label_style = "  error:", STYLE.status_danger
    else
      label, label_style = "  output:", STYLE.footer
    end
    rows[#rows + 1] = tui.text { content = label, style = label_style, wrap = "none" }
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

-- Plan entries carry a `submitted_at` timestamp the lead-workflow
-- actor stamps when the write-review tool fires. Render as "HH:MM"
-- for the plan-box subtitle. Accepts ISO 8601 strings (e.g.
-- "2026-05-08T14:30:21Z") and epoch-ms numbers; anything else
-- stringifies as-is so a malformed value doesn't crash the surface.
local function format_submitted_at(s)
  if type(s) == "number" then
    return os.date("!%H:%M", math.floor(s / 1000))
  end
  if type(s) ~= "string" then return tostring(s) end
  local hh, mm = s:match("T(%d%d):(%d%d)")
  if hh ~= nil then return hh .. ":" .. mm end
  return s
end

-- Plan entry: full-width yellow-bordered block carrying a write-review
-- plan the lead-workflow actor submitted. Render-only — the model
-- already saw the plan via the tool call's args, so chat.lua does NOT
-- forward the body into model context (the submit reducer's
-- `chat.input.submit` path doesn't carry plan content; the plan
-- envelope is consumed here without echoing as a `chat.message.append`).
-- Layout:
--   ╭── plan · submitted at HH:MM ──╮
--   │ <markdown body>               │
--   │                               │
--   │ [/approve to proceed | /reject <reason>]
--   ╰───────────────────────────────╯
-- Status drives the border style: pending = yellow active, approved =
-- yellow italic with green check subtitle, rejected = red strikethrough
-- with red status subtitle. The hint row only renders for `pending`.
local function render_plan_entry(entry)
  local status = entry.status or "pending"
  local border_style
  if status == "approved" then
    border_style = STYLE.plan_chrome_approved
  elseif status == "rejected" then
    border_style = STYLE.plan_chrome_rejected
  else
    border_style = STYLE.plan_chrome
  end

  local subtitle_text = "plan"
  local stamped = format_submitted_at(entry.submitted_at)
  if stamped and stamped ~= "" then
    subtitle_text = subtitle_text .. " · submitted at " .. stamped
  end

  local rows = {
    tui.text { content = subtitle_text, style = STYLE.plan_subtitle, wrap = "none" },
    md(entry.text or ""),
  }

  if status == "pending" then
    rows[#rows + 1] = tui.text {
      content = "[/approve to proceed | /reject <reason> to send back]",
      style   = STYLE.plan_hint,
      wrap    = "word",
    }
  elseif status == "approved" then
    rows[#rows + 1] = tui.text {
      content = "✓ approved",
      style   = STYLE.plan_status_approved,
      wrap    = "none",
    }
  elseif status == "rejected" then
    rows[#rows + 1] = tui.text {
      content = "✗ rejected",
      style   = STYLE.plan_status_rejected,
      wrap    = "none",
    }
  end

  return bordered_box(
    tui.column { gap = 0, children = rows },
    border_style
  )
end

local function render_entry(entry, _i, expanded)
  if entry.kind == "tool_call" then
    if expanded then return tool_expanded(entry) end
    return tool_collapsed(entry)
  end
  if entry.kind == "plan" then
    return render_plan_entry(entry)
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

-- Tool permission popup (spec section 12). Border is HL_STATUS_WARN;
-- footer reads `[A]pprove [D]eny (ESC = deny)`. Keyhandlers in update.
local function popup_tool_permission(state)
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
local CURSOR_ROW_STYLE = { fg = "#000000", bg = C.user }

local function popup_session_picker(state)
  if not state.popup or state.popup.variant ~= "session_picker" then return nil end
  local p = state.popup
  local sessions = p.sessions or {}
  local empty_child = tui.column { gap = 0, children = {
    tui.text {
      content = "No saved sessions found.",
      style   = STYLE.status_dim, wrap = "word",
    },
    tui.text {
      content = "Sessions live at " .. (session_dir() or "<unknown>"),
      style   = STYLE.status_dim, wrap = "word",
    },
  }}
  local picker_body
  if #sessions == 0 then
    picker_body = empty_child
  else
    picker_body = W.picker.view({
      state        = { cursor = p.cursor or 1 },
      entries      = function() return sessions end,
      format_entry = function(s)
        local stamp = format_started_at(s.started_at)
        local preview = clip_preview(s.preview, 50)
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
  local prov_w = 0
  for _, e in ipairs(matches) do
    if e.provider and #e.provider > prov_w then prov_w = #e.provider end
  end
  if prov_w > 20 then prov_w = 20 end

  local empty_text
  if awaiting_count(p.awaiting) == 0 and (p.models == nil or #p.models == 0) then
    empty_text = "No providers connected.\nWire one up in init.lua (see docs/provider-plugins.md)."
  end

  local picker_body = W.picker.view({
    state          = { cursor = p.cursor or 1, query = p.query or "" },
    entries        = function() return p.models or {} end,
    filter         = function(_, q) return model_picker_filter(p.models, q) end,
    format_entry   = function(e)
      return string.format("%-" .. prov_w .. "s  %s", e.provider or "?", e.model or "?")
    end,
    cursor_style   = CURSOR_ROW_STYLE,
    row_style      = STYLE.status,
    search_style   = STYLE.status,
    divider_style  = STYLE.footer,
    empty_style    = STYLE.status_dim,
    empty_text     = empty_text,
    cap            = 12,
    gap            = 0,
  })

  local awaiting_n = awaiting_count(p.awaiting)
  local children = compact {
    picker_body,
    (awaiting_n > 0) and tui.text {
      content = string.format("loading from %d provider(s)…", awaiting_n),
      style   = STYLE.status_dim,
      wrap    = "none",
    } or nil,
    tui.text {
      content = "↑/↓ select · Enter pick · Esc close · type to filter",
      style   = STYLE.status_dim,
      wrap    = "none",
    },
  }

  return W.popup.view({
    open         = true,
    border_style = STYLE.popup_user,
    width        = "60%",
    height       = "60%",
    scroll_key   = "popup_model_picker",
    title        = "── pick a model ──",
    title_style  = STYLE.popup_user,
    child        = tui.column { gap = 1, children = children },
  })
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
-- slash + @-path autocomplete (inline above input)
------------------------------------------------------------------------
--
-- Both dropdowns reuse `W.picker.view` — the picker widget renders a
-- windowed list with cursor-row inversion (same shape as the legacy
-- inline popup). Slash filtering and @-path entry resolution are
-- driven by the reducer's `state.slash.matches` / `state.at_complete.
-- matches` (computed by `refresh_slash` / `refresh_at_complete`),
-- which keeps this view-time call purely cosmetic.

local function inline_picker_list(matches, cursor, format_entry, empty_text)
  if #matches == 0 then
    return tui.text {
      content = empty_text or "no matches",
      style   = STYLE.status_dim,
      wrap    = "none",
    }
  end
  return W.picker.view({
    state        = { cursor = cursor or 1 },
    entries      = function() return matches end,
    format_entry = format_entry,
    cursor_style = CURSOR_ROW_STYLE,
    show_search  = false,
    cap          = 8,
    gap          = 0,
  })
end

local function slash_autocomplete_inline(state)
  if not state.slash then return nil end
  return inline_picker_list(
    state.slash.matches or {}, state.slash.cursor,
    function(cmd) return string.format("/%-12s  %s", cmd.name, cmd.hint or "") end,
    "no matching commands"
  )
end

local function at_autocomplete_inline(state)
  if not state.at_complete then return nil end
  return inline_picker_list(
    state.at_complete.matches or {}, state.at_complete.cursor,
    function(e)
      -- Trailing `/` on directories so the user can see at a glance
      -- which entries are drillable.
      return e.is_dir and (e.name .. "/") or e.name
    end,
    "no matching paths"
  )
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
    local drop = false
    if run.completed_at_ms ~= nil
       and (now_ms - run.completed_at_ms) > DAG_LINGER_MS then
      drop = true
    end
    if drop then
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
      -- Drag-to-select scopes to this column. The sidebar doesn't
      -- scroll, so the selection's content geometry equals the
      -- column's painted rect — the engine paints the column into
      -- a rect-sized scratch buffer and extracts plain text from
      -- the cells the drag covered. Keyed so the engine can
      -- re-resolve the captured widget across `view` rebuilds.
      child = tui.column {
        gap        = 0,
        key        = "sidebar",
        selectable = true,
        children   = children,
      },
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

-- Welcome-banner copy painted on a truly fresh chat surface (no entries,
-- no in-flight turn). Disappears the instant the user submits — the
-- entries-non-empty branch below skips it.
--
-- To disable: set WELCOME_BANNER_LINES to {} (empty list). To customize:
-- edit the strings; each entry is one centered line. Style is shared
-- with other dim chrome via STYLE.status_dim, so palette tweaks land
-- here automatically.
local WELCOME_BANNER_LINES = {
  "Welcome to the starter config!",
  "",
  "This is your average agentic workflow, but nefor can do much more than that.",
  "",
  "Experiment and do whatever you want!",
}

local function welcome_banner()
  if #WELCOME_BANNER_LINES == 0 then return nil end
  local rows = {}
  for i, line in ipairs(WELCOME_BANNER_LINES) do
    -- Each line: a 1-row-tall `tui.align{center}` so the line centers
    -- horizontally within the chat column. The height clamp is
    -- load-bearing — without it `tui.align` greedily takes the
    -- available height (it fills its slot at parent max), which would
    -- collapse subsequent rows to zero height. With max_height=1 each
    -- align resolves to exactly one row of centered content.
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
  -- Outer wrapper centers the block vertically within the transcript
  -- pane: the parent gives us the full available height and `tui.align`
  -- parks the fixed-height row stack in the middle.
  return tui.align {
    alignment = "center",
    child = tui.column { gap = 0, children = rows },
  }
end

local function transcript(state)
  -- Welcome banner shows on a fresh surface only; the chat widget's
  -- `empty_view` slot accepts a fn returning the banner tree, which it
  -- stacks over an empty scrollable so scroll_position keeps resolving.
  -- Replay-mode opt-out: between sessions.replay.start and the first
  -- replayed chat.message.append, the transcript is briefly empty AND
  -- we're rebuilding. Painting the banner here would flash the welcome
  -- copy in the middle of a resume.
  local empty_view
  if state.in_flight == nil and not state.pending and not state.replay_mode then
    empty_view = welcome_banner
  end
  return W.chat.view({
    key          = "transcript",
    entries      = function() return state.entries or {} end,
    render_entry = function(e, i)
      return render_entry(e, i, state.expanded_details)
    end,
    append       = thinking_widget(state),
    empty_view   = empty_view,
  })
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
  -- Input renders as a bordered text input. The prompt widget's
  -- completion plumbing isn't used here — chat.lua maintains its own
  -- slash / at-path completion state in the reducer with bespoke
  -- behaviour (filesystem source for @, command registry for /, custom
  -- apply semantics). The autocomplete dropdowns are rendered inline
  -- above the input via slash_autocomplete_inline / at_autocomplete_inline
  -- and produce the same column layout the prompt widget would.
  local input_field = W.prompt.view({
    state          = { value = state.input_value },
    key            = "input",
    focused        = input_focused,
    on_change      = "input.changed",
    on_submit      = "input.submit",
    border_style   = input_border_style,
    border_key     = "input-field",
    min_lines      = 1,
    max_lines      = 6,
    selectable     = true,
  })

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
      at_autocomplete_inline(state),
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
  local terminal_status
  if has_output then terminal_status = "done"
  elseif has_error then terminal_status = "error"
  else terminal_status = "error" end
  -- Drop results for nodes we haven't observed dispatch for. In live
  -- mode this shouldn't happen; if it does, the result will be visible
  -- in logs and that's the right place to investigate, not a synthetic
  -- panel entry that papers over the gap.
  if not (state.dag_runs and state.dag_runs[run_id]
      and state.dag_runs[run_id].nodes
      and state.dag_runs[run_id].nodes[node_id]) then
    return state
  end
  return dag_apply(state, run_id, function(prev)
    local nodes = {}
    for k, v in pairs(prev.nodes or {}) do nodes[k] = v end
    local node = nodes[node_id]
    nodes[node_id] = shallow_merge(node, {
      status = terminal_status, finished_at_ms = now_ms,
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
-- @path preprocessor (#47)
------------------------------------------------------------------------
-- Inline file references like `@starter/chat.lua` into the user's
-- submitted text BEFORE it reaches the provider. The lead workflow
-- spec (lead-workflow-spec §1, §6, §8) treats this as a starter-config
-- prerequisite: the orchestrator's first turn sees the file contents
-- already inlined; large files truncate with a marker pointing at the
-- existing `read_file` tool for the full contents.
--
-- Scope (intentionally small):
--   * pattern is `@<non-whitespace>`; trailing common punctuation
--     (`.,;:!?)`) is shaved off the captured token because it almost
--     never belongs to the path and the user's prompt-tail punctuation
--     would otherwise turn `@a.lua.` into a missing-file silent no-op.
--   * resolution: cwd-relative first, then treat the token as already
--     absolute. Paths that don't resolve / can't be opened leave the
--     `@<token>` as-is — no error surfaces, the user sees the raw text
--     in their bubble and the model can ask.
--   * inlined block is a fenced HTML-ish `<file path="…">` … `</file>`
--     wrapper. Code-fence language is inferred from extension; unknown
--     extensions render with a plain ``` fence.

local AT_PATH_INLINE_BUDGET = 16 * 1024
local AT_PATH_TRUNCATION_MARKER =
  "\n... [truncated; use read_file tool for full contents] ..."

local AT_PATH_FENCE_LANG = {
  lua = "lua", rs = "rust", md = "md", json = "json", toml = "toml",
  py = "python", js = "javascript", ts = "typescript", tsx = "tsx",
  sh = "bash", bash = "bash", yaml = "yaml", yml = "yaml",
  html = "html", css = "css", go = "go", rb = "ruby", java = "java",
}

local function at_path_fence_lang(path)
  local ext = path:match("%.([%w]+)$")
  if ext == nil then return "" end
  return AT_PATH_FENCE_LANG[ext:lower()] or ""
end

local function at_path_read(path)
  local f = io.open(path, "r")
  if f == nil then return nil end
  local data = f:read(AT_PATH_INLINE_BUDGET + 1)
  f:close()
  if data == nil then return "" end
  if #data > AT_PATH_INLINE_BUDGET then
    return data:sub(1, AT_PATH_INLINE_BUDGET) .. AT_PATH_TRUNCATION_MARKER
  end
  return data
end

-- Try cwd-relative first, then treat the path as already absolute.
-- Both branches can resolve the same string when cwd happens to be `/`,
-- but io.open is idempotent so the duplication is harmless.
local function at_path_resolve(token)
  local data = at_path_read(token)
  if data ~= nil then return data, token end
  if token:sub(1, 1) == "/" then return nil, nil end
  return nil, nil
end

local function expand_at_path_refs(text)
  if text == nil or text == "" or text:find("@", 1, true) == nil then
    return text
  end
  return (text:gsub("@([^%s]+)", function(token)
    -- Strip trailing prompt punctuation that almost never belongs to a
    -- path (`.`, `,`, `;`, `:`, `!`, `?`, `)`). One pass — multiple
    -- trailing punctuation chars (e.g. `@file.lua?!`) all peel off.
    local trimmed, _trail_n = token:gsub("[%.%,;:!%?%)]+$", "")
    if trimmed == "" then return nil end
    local data, resolved = at_path_resolve(trimmed)
    if data == nil then return nil end
    local trail = token:sub(#trimmed + 1)
    local lang = at_path_fence_lang(resolved)
    return string.format(
      "<file path=\"%s\">\n```%s\n%s\n```\n</file>%s",
      resolved, lang, data, trail
    )
  end))
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

local function refresh_at_complete(state, text)
  local token, _at_pos, body = active_at_token(text)
  if token == nil then
    return shallow_merge(state, { at_complete = NIL_SENTINEL })
  end
  local base_dir, leaf = split_at_body(body or "")
  local prev = state.at_complete
  if prev and prev.token == token then return state end
  -- Reuse the cached entry list per base_dir. The dir cache hangs
  -- off `at_complete.dir_cache` so when the user backs up out of a
  -- subdir (e.g. types past `@/private/var/` then deletes back to
  -- `@/private/`) we don't re-shell out to `ls` for a directory we
  -- already enumerated. Saves an io.popen per leaf-only keystroke
  -- AND per back-step over previously visited base_dirs.
  local dir_cache = (prev and prev.dir_cache) or {}
  local entries = dir_cache[base_dir]
  if entries == nil then
    entries = ls_entries(resolve_base_dir(base_dir))
    dir_cache[base_dir] = entries
  end
  local matches = at_filter(entries, leaf)
  local at = {
    matches   = matches,
    cursor    = 1,
    token     = token,
    base_dir  = base_dir,
    leaf      = leaf,
    entries   = entries,
    dir_cache = dir_cache,
  }
  return shallow_merge(state, { at_complete = at })
end

-- Apply the cursor-selected entry: replace the trailing `@<token>`
-- with `@<base_dir><name>` (plus a trailing `/` for directories so
-- the user can keep drilling). Returns the new input value, or
-- `nil` if there's nothing to apply (no entries / no active token).
local function apply_at_completion(state)
  local at = state.at_complete
  if at == nil then return nil end
  local entry = at.matches and at.matches[at.cursor or 1]
  if entry == nil then return nil end
  local text = state.input_value or ""
  local token, at_pos = active_at_token(text)
  if token == nil or at_pos == nil then return nil end
  local replacement = "@" .. (at.base_dir or "") .. entry.name
  if entry.is_dir then replacement = replacement .. "/" end
  return text:sub(1, at_pos - 1) .. replacement
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
    state = refresh_at_complete(state, v)
    return state, {}
  end

  if kind == "input.submit" then
    local text = msg.value or ""
    -- Note on @-path autocomplete + Enter: slash submits the highlighted
    -- match because the slash command IS the action; @-paths are file
    -- references embedded in a wider message, so Enter is overwhelmingly
    -- "send my message" — Tab is the right key to insert a completion.
    -- The popup is informational, not modal: closes on submit.
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
        pending = false, slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
        dag_runs = {}, firing_to_node = {},
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
        input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
        popup = { variant = "help" },
      }), {}
    end
    if cmd == "yolo" then
      local s = shallow_merge(state, { input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "yolo" } },
      }
    end
    if cmd == "safe" then
      local s = shallow_merge(state, { input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "normal" } },
      }
    end
    if cmd == "login" or cmd == "logout" then
      local body = { kind = "chat." .. cmd .. "_requested" }
      if args and #args > 0 then body.provider = args end
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL }), {
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
        return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL }), {
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
        input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
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
      --
      -- Locally clear the transcript here rather than waiting for the
      -- session_end bus envelope to do it — the session_end handler
      -- deliberately doesn't touch `entries` (see comment there) so
      -- that user keystrokes between `/new` (or `/resume`) and the
      -- broker's lifecycle round-trip aren't silently wiped. Replay
      -- arrives via push_entry on each `chat.message.append` and
      -- rebuilds the view from empty.
      if args and #args > 0 then
        local id = args:match("^([%w%-]+)") or args
        return shallow_merge(state, {
          input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending = false, dag_runs = {}, firing_to_node = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
        }), {
          emit_resume_request(id),
        }
      end
      -- `/resume` (no args) — open the picker.
      local sessions = list_recent_sessions(10)
      return shallow_merge(state, {
        input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
        popup = {
          variant  = "session_picker",
          sessions = sessions,
          cursor   = 1,
        },
      }), {}
    end
    if cmd ~= nil then
      -- Unknown slash → generic chat.command for user-defined Lua handlers.
      return shallow_merge(state, { input_value = "", slash = NIL_SENTINEL, at_complete = NIL_SENTINEL }), {
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
    --
    -- `@path` preprocessor (#47): the wire envelope, the local user
    -- bubble, and the dedup marker all carry the EXPANDED text. The
    -- user sees exactly what the model receives (transparency); the
    -- orchestrator's echo round-trips through the same dedup gate.
    -- Prompt-history (recall via arrow-up) keeps the ORIGINAL `@`-form
    -- so a recalled prompt edits like a fresh one and re-expands at
    -- next submit (file contents may have changed in the meantime —
    -- the user re-submitting expects the current state, not a
    -- snapshot from the original turn).
    local wire_text = expand_at_path_refs(text)
    local with_user = push_entry(state, { role = "user", text = wire_text, kind = "text" })
    -- Prepend to prompt_history (newest at index 1) and cap. History
    -- recall reads from index 1, so prepending keeps the cursor model
    -- simple — Up = older = larger index, Down = newer = smaller.
    -- Mirror to disk so the entry survives a nefor restart (issue #39).
    local history = { text }
    for i, v in ipairs(state.prompt_history or {}) do
      if i >= INPUT_HISTORY_MAX then break end
      history[#history + 1] = v
    end
    persist_input_history(history)
    local cleared = shallow_merge(with_user, {
      input_value = "", pending = true,
      turn_started_at = tui.now_ms(), slash = NIL_SENTINEL, at_complete = NIL_SENTINEL,
      prompt_history = history,
      history_cursor = NIL_SENTINEL,
      -- Mark the next bus-delivered chat.message.append with this
      -- exact text + role as the orchestrator's persist-echo and
      -- swallow it. Cleared after one match — sequential identical
      -- submits each set their own marker on submit, so the second
      -- echo doesn't get eaten by the first marker.
      pending_user_echo = wire_text,
    })
    -- Re-pin the transcript to the bottom: stick_to = "end" only
    -- auto-follows new content while `was_at_end` is true, so a user
    -- who scrolled up to read older context and submits a new prompt
    -- would otherwise stay parked mid-transcript watching their fresh
    -- message + the incoming response render off-screen below the
    -- viewport. scroll_into_view flips the flag on the next paint so
    -- the auto-follow re-engages for the streaming response too.
    tui.scroll_into_view("transcript")
    return cleared, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = wire_text } },
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
    -- 2b) close @-path autocomplete
    if state.at_complete then
      return shallow_merge(state, { at_complete = NIL_SENTINEL }), {}
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
  -- Model + session picker popups: delegate cursor/filter handling to
  -- W.picker.handle. Each popup's state lives under `state.popup`;
  -- when a handler returns, we fold its state patch into the popup
  -- slot and emit effects through the caller-side on_select callback.
  -- We gate the picker delegation on key.* events so non-key messages
  -- (chat.models.listed, chat.model.set_ack, etc.) keep flowing to
  -- their dedicated handlers below.
  if state.popup and state.popup.variant == "model_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local result = W.picker.handle({
      state   = { cursor = p.cursor or 1, query = p.query or "" },
      entries = function() return p.models or {} end,
      filter  = function(_, q) return model_picker_filter(p.models, q) end,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil then
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = {
              kind     = "chat.model.set",
              provider = result.selected.provider,
              model    = result.selected.model,
            } },
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
  end

  -- Session picker: same shape as model picker, no filter input.
  -- Esc handled in the popup-close branch above (closes without
  -- emitting). All other keys swallow so they don't bubble.
  if state.popup and state.popup.variant == "session_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local sessions = p.sessions or {}
    local result = W.picker.handle({
      state       = { cursor = p.cursor or 1 },
      entries     = function() return sessions end,
      show_search = false,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil and result.selected.id then
        return shallow_merge(state, {
          popup = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending = false, dag_runs = {}, firing_to_node = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
        }), {
          emit_resume_request(result.selected.id),
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
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

  -- @-path autocomplete keys (when popup open). Up/Down move the
  -- cursor; Tab inserts the highlighted entry into the input,
  -- replacing the trailing `@<token>`. Enter is handled in the
  -- input.submit branch above (it inserts, not submits, when the
  -- popup is open).
  if state.at_complete then
    if kind == "key.up" then
      local n = #(state.at_complete.matches or {})
      if n == 0 then return state, {} end
      local cur = state.at_complete.cursor or 1
      cur = cur - 1
      if cur < 1 then cur = n end
      return shallow_merge(state, {
        at_complete = shallow_merge(state.at_complete, { cursor = cur }),
      }), {}
    end
    if kind == "key.down" then
      local n = #(state.at_complete.matches or {})
      if n == 0 then return state, {} end
      local cur = state.at_complete.cursor or 1
      cur = cur + 1
      if cur > n then cur = 1 end
      return shallow_merge(state, {
        at_complete = shallow_merge(state.at_complete, { cursor = cur }),
      }), {}
    end
    if kind == "key.tab" then
      local new_text = apply_at_completion(state)
      if new_text == nil then return state, {} end
      local s = shallow_merge(state, { input_value = new_text })
      s = refresh_at_complete(s, new_text)
      return s, {}
    end
  end

  -- Scroll keys: route to the active popup's scrollable when a popup is
  -- open; otherwise to the transcript. The popup's body is wrapped in
  -- `tui.scrollable { key = "popup_<variant>" }` (see `bordered_popup`),
  -- so the same `tui.scroll_*` API drives both. Disabling transcript
  -- scrolling while a popup is up matches the user-expected "modal
  -- focus" gesture — the popup owns the keyboard while it's visible.
  -- Scroll-key routing: when a popup is open, route scroll keys to its
  -- inner scrollable; otherwise to the transcript. Popups are wrapped
  -- by W.popup.view with `scroll_key = "popup_<variant>"`, so the same
  -- `tui.scroll_*` API drives both. Up/Down on the transcript surface
  -- ALSO drive prompt-history recall when no popup is active and the
  -- input is empty / already navigating — handled separately below.
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

  local function route_scroll(delta_or_fn)
    local target = active_scroll_key() or "transcript"
    delta_or_fn(target)
  end

  if kind == "key.pageup" then
    route_scroll(function(k) tui.scroll_by(k, -10) end)
    return state, {}
  end
  if kind == "key.pagedown" then
    route_scroll(function(k) tui.scroll_by(k, 10) end)
    return state, {}
  end
  if kind == "key.up" then
    if active_scroll_key() == nil then
      local navigating = state.history_cursor ~= nil
      local empty = (state.input_value or "") == ""
      if (navigating or empty) and #(state.prompt_history or {}) > 0 then
        local cur = state.history_cursor or 0
        local nxt = math.min(cur + 1, #state.prompt_history)
        return shallow_merge(state, {
          input_value    = state.prompt_history[nxt],
          history_cursor = nxt,
        }), {}
      end
    end
    route_scroll(function(k) tui.scroll_by(k, -1) end)
    return state, {}
  end
  if kind == "key.down" then
    if active_scroll_key() == nil and state.history_cursor ~= nil then
      local cur = state.history_cursor
      if cur <= 1 then
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
    route_scroll(function(k) tui.scroll_by(k, 1) end)
    return state, {}
  end
  if kind == "key.home" then
    route_scroll(function(k) tui.scroll_to(k, 0) end)
    return state, {}
  end
  if kind == "key.end" then
    route_scroll(function(k) tui.scroll_into_view(k) end)
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
    -- Tear down ephemeral turn state — but DO NOT touch `entries`.
    --
    -- Rationale: session_end arrives via the bus, on a different
    -- broker tick than the user's keystroke that triggered it (a
    -- /new or /resume submit). If the user typed their first prompt
    -- in the new session before this envelope landed, `entries`
    -- already holds their locally-pushed message — and wiping it
    -- here is exactly the race that made the user's prompt
    -- invisible while the orchestrator's tool_call still painted.
    -- The transcript clear is owned by the trigger paths instead:
    -- /new clears locally in its slash-command handler; /resume
    -- clears locally too (see the picker-Enter and `/resume <id>`
    -- arms). For a /resume that lands replay envelopes, push_entry
    -- on each replayed `chat.message.append` rebuilds the prior
    -- view directly — no wipe needed here.
    --
    -- pending_user_echo is preserved for a similar reason: if the
    -- user submitted text moments ago, the echo's defensive dedup
    -- (chat.message.append handler) is what protects against
    -- double-rendering. Clearing the marker here would mean the
    -- echo arrives, finds no marker, and unconditionally pushes —
    -- producing the OPPOSITE bug (user line rendered twice). The
    -- marker self-clears the moment the echo lands or the next
    -- /new fires.
    return shallow_merge(state, {
      in_flight        = NIL_SENTINEL,
      pending          = false,
      turn_started_at  = NIL_SENTINEL,
      last_turn_duration_ms = NIL_SENTINEL,
      popup            = NIL_SENTINEL,
      toast            = NIL_SENTINEL,
      slash            = NIL_SENTINEL,
      dag_runs         = {},
    }), {}
  end

  if kind == "sessions.session_start" then
    -- `dag_runs` always cleared: a fresh session is a fresh
    -- DAG-context boundary regardless of which path we took to
    -- get here. session_end's clear isn't enough on its own —
    -- if a session swap doesn't go cleanly through session_end,
    -- or if a run was mid-flight when the swap fired, stale runs
    -- would otherwise stack on top of new ones in the panel.
    --
    -- Boot path: state is already empty; the entries-wipe that
    -- USED to live here turned out to break ncp.lua's replay-on-attach
    -- (boot session_start delivered AFTER the user's first prompt
    -- nuked the local-push), so we deliberately do nothing for
    -- entries here. dag_runs is independent of that race (no
    -- pre-boot dispatch path) so we still clear it.
    --
    -- Replay-mode flip is driven by `sessions.replay.start` /
    -- `sessions.replay.end` markers below — the framing-marker
    -- contract (Phase 4.5) is the canonical replay window now.
    return shallow_merge(state, { dag_runs = {}, firing_to_node = {} }), {}
  end

  if kind == "sessions.replay.start" then
    -- Replay started — suppress UI side effects that would re-trigger
    -- against envelopes the user already saw the first time round.
    -- Notable example: the tool.permission_request popup. The user
    -- already approved in the original session (its decision is
    -- recorded in the jsonl); a fresh popup would be a re-prompt.
    return shallow_merge(state, { replay_mode = true }), {}
  end

  if kind == "sessions.replay.end" then
    -- Replay finished — flip back to live so future envelopes drive
    -- popups and other side effects normally again. The next render
    -- fires from the surrounding update loop.
    return shallow_merge(state, { replay_mode = NIL_SENTINEL }), {}
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
    -- System messages always indicate the turn ended (interrupted,
    -- error from the provider, etc.) — clear the thinking spinner
    -- and turn-elapsed counter so the UI doesn't sit on
    -- "[thinking... Ns]" forever after the orchestrator gives up.
    local turn_state = role == "system"
      and { pending = false, turn_started_at = NIL_SENTINEL }
      or  {}
    return push_entry(shallow_merge(state, turn_state), {
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

  -- ── plan-message contract (lead-workflow `write-review` tool) ──────
  -- The lead-workflow actor's `write-review` tool fires a plan envelope
  -- the chat surface renders as a yellow-bordered "plan" entry. This
  -- block is render-only on chat.lua's side — the plan body is NOT
  -- added to model context by anything in this file (the submit
  -- reducer's `chat.input.submit` emit carries only the user's typed
  -- text). The model already saw the plan as the tool call's args, so
  -- re-forwarding it via `chat.message.append` would be a duplication.
  if kind == "chat.plan.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    -- Idempotent on plan_id: the lead-workflow actor's plan.submitted
    -- reducer fires chat.plan.append on every handling — live (via bus
    -- feedback) AND replay. Sessions persists the live emission, then
    -- /resume replays both the persisted chat.plan.append and the
    -- re-emit from the actor's reducer. Without this guard the same
    -- plan_id would produce two yellow boxes after every /resume. Drop
    -- the duplicate; status stays untouched (already-approved entries
    -- don't regress to pending).
    local plan_id = msg.plan_id or ""
    if plan_id ~= "" then
      for _, v in ipairs(state.entries) do
        if v.kind == "plan" and v.plan_id == plan_id then
          return state, {}
        end
      end
    end
    return push_entry(state, {
      kind         = "plan",
      plan_id      = plan_id,
      text         = text,
      submitted_at = msg.submitted_at,
      status       = "pending",
    }), {}
  end

  -- Approval/rejection arrives from the lead-workflow actor after the
  -- user types `/approve` or `/reject`. We update the matching plan
  -- entry's status in place — visual state changes (border colour,
  -- check/cross subtitle) but the plan stays in the transcript so the
  -- user can scroll back to see what was decided.
  if kind == "lead-workflow.plan.approved" then
    local plan_id = msg.plan_id or ""
    if plan_id == "" then return state, {} end
    local approved = (msg.approved == true)
    local entries = {}
    local matched = false
    for i, v in ipairs(state.entries) do
      if not matched and v.kind == "plan" and v.plan_id == plan_id then
        entries[i] = shallow_merge(v, {
          status = approved and "approved" or "rejected",
        })
        matched = true
      else
        entries[i] = v
      end
    end
    if not matched then return state, {} end
    return shallow_merge(state, { entries = entries }), {}
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
    -- Replayed set_ack envelopes carry the model the OLD session was
    -- bound to — replaying them onto the live state would clobber
    -- whatever the user set via /model in the LIVE session before the
    -- /resume (chat.model.set_ack is persisted, so the original
    -- session's mock-provider hello → set_ack lives in the jsonl).
    -- Bug A7 manifested as: pick mock → /new → /model qwen → /resume
    -- prior mock chat → status bar reverts to mock-model even though
    -- the orchestrator's live config (and the next reply's provider)
    -- is qwen. The agentic-loop owns the live provider/model; it
    -- doesn't replay chat.model.set on its own input gate, so its
    -- state stays correct. chat.lua mirrors that posture by ignoring
    -- replayed set_ack envelopes — only LIVE ones drive the badge.
    if state.replay_mode then return state, {} end
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
    -- Replay path: the user already approved in the original session
    -- and the decision is in the jsonl — popping a fresh approval
    -- popup would be a re-prompt for the same call. Drop the request
    -- silently; the matching tool.permission_response is also in the
    -- jsonl and will replay through tool-gate's normal handler.
    if state.replay_mode then return state, {} end
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

  -- DAG observation. Each handler short-circuits during replay: the
  -- graph.* envelopes seeded into the resumed session's jsonl are
  -- snapshots from the prior live run, not fresh dispatches, and
  -- mutating dag_runs from them would re-light a panel that should
  -- start clean (sessions.session_start clears it). Mirrors the
  -- chat.tool.permission_request guard above.
  --
  -- Wire shape post Phase 3b: reasoner-graph emits
  --   * graph.run_started  { run_id, total_nodes }
  --   * graph.node.fired   { run_id, node_id, firing_id, reasoner }
  --     — paired observer for each tool.invoke dispatch.
  --   * tool.result        { id, result | error }
  --     — id == firing_id closes one node; id == run_id closes the run.
  --   We also keep a firing_id → (run_id, node_id) map per state so
  --   tool.result events can be routed back to the right node without
  --   parsing dispatch traffic.
  if kind == "graph.run_started" then
    if state.replay_mode then return state, {} end
    local now = tui.now_ms()
    return dag_run_started(state, msg.run_id or "", msg.total_nodes or 0, now), {}
  end
  if kind == "graph.node.fired" then
    if state.replay_mode then return state, {} end
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    if (msg.firing_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    local with_dispatch = dag_node_dispatched(state, msg.run_id, msg.node_id, msg.reasoner or "", now)
    local prev_map = with_dispatch.firing_to_node or {}
    local next_map = {}
    for k, v in pairs(prev_map) do next_map[k] = v end
    next_map[msg.firing_id] = { run_id = msg.run_id, node_id = msg.node_id }
    return shallow_merge(with_dispatch, { firing_to_node = next_map }), {}
  end
  if kind == "tool.result" then
    if state.replay_mode then return state, {} end
    local id = msg.id
    if type(id) ~= "string" or id == "" then return state, {} end
    local now = tui.now_ms()
    -- Run-close: id matches a tracked run.
    if state.dag_runs and state.dag_runs[id] then
      local result = msg.result
      local status, results
      if type(result) == "table" then
        status  = result.status
        results = result.results
      end
      return dag_run_complete(state, id, status, results, now), {}
    end
    -- Per-firing close: look up firing_id → (run_id, node_id) map.
    local map_entry = (state.firing_to_node or {})[id]
    if map_entry then
      local run_id  = map_entry.run_id
      local node_id = map_entry.node_id
      local has_output = msg.result ~= nil
      local has_error  = msg.error  ~= nil
      local next_state = dag_node_result(state, run_id, node_id, has_output, has_error, now)
      local next_map = {}
      for k, v in pairs(state.firing_to_node or {}) do next_map[k] = v end
      next_map[id] = nil
      return shallow_merge(next_state, { firing_to_node = next_map }), {}
    end
    return state, {}
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
