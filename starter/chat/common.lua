-- Shared chrome + formatting helpers for the chat surface. Holds the
-- palette, named styles, markdown theme, and the small set of pure
-- utilities every chat submodule needs (compact, pretty_json,
-- humanize_*). Submodules require this directly rather than via the
-- entry file so they're self-contained — adding a new sibling doesn't
-- need a re-export plumbing pass.

local tui_lib = require("nefor-tui")
local util    = tui_lib.util

local M = {}

M.shallow_merge   = util.shallow_merge
M.NIL_SENTINEL    = util.NIL
M.bordered_box    = util.bordered_box

function M.normalize_chat_state(state)
  state = type(state) == "table" and state or {}
  local patch = {}

  local function ensure_table(key, default)
    if type(state[key]) ~= "table" then
      patch[key] = default or {}
      return patch[key]
    end
    return state[key]
  end

  local entries = ensure_table("entries")
  ensure_table("stats")
  ensure_table("auth")
  ensure_table("supports_login")
  ensure_table("dag_runs")
  ensure_table("firing_to_node")
  ensure_table("toasts")
  ensure_table("prompt_history")

  if state.popup_queue ~= nil and type(state.popup_queue) ~= "table" then
    patch.popup_queue = M.NIL_SENTINEL
  end

  local queued = state.queued_entry_idx
  if queued ~= nil
      and (type(queued) ~= "number" or queued < 1 or entries[queued] == nil) then
    patch.queued_entry_idx = M.NIL_SENTINEL
  end

  local in_flight = state.in_flight
  if in_flight ~= nil
      and (type(in_flight) ~= "number" or in_flight < 1 or entries[in_flight] == nil) then
    patch.in_flight = M.NIL_SENTINEL
  end

  for _ in pairs(patch) do
    return M.shallow_merge(state, patch)
  end
  return state
end

-- Resolve the engine's data root from env vars. Must match
-- `nefor.fs.data_root()` exactly so readers and writers agree on the
-- on-disk location. Cascade, first hit wins:
--   1. NEFOR_DATA_DIR — canonical setting (engine always propagates
--      its resolved value into this env var when spawning plugins).
--   2. XDG_DATA_HOME/nefor — standard XDG.
--   3. $HOME/.local/share/nefor — XDG default fallback.
-- Returns nil only when NEFOR_DATA_DIR and XDG_DATA_HOME and HOME are
-- all unset (no sane default available).
--
-- The chat surface runs inside the nefor-tui plugin's Lua VM where the
-- engine `nefor.fs.*` bindings aren't installed; this resolver is a
-- matching reimplementation rather than a delegation. Centralised here
-- so the session picker and the input-history file stay in sync.
function M.data_root()
  local override = os.getenv("NEFOR_DATA_DIR")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

-- Drop nil holes from an array so reducer-built children lists stay
-- dense. Lua's table constructor with conditionally-nil entries leaves
-- gaps that break `ipairs`; the renderer relies on contiguous indices.
function M.compact(list)
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
-- the tool expanded view to render structured `input` payloads.
-- Strings are quoted, numbers and booleans render verbatim, nested
-- tables nest one indent level. Arrays vs objects are distinguished
-- by whether the table has a numeric `[1]` key (the JSON-decoded
-- shape from nefor-protocol is reliable).
function M.pretty_json(value, indent)
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
  local n = 0
  for _ in pairs(value) do n = n + 1 end
  if n == 0 then
    return "{}"
  end
  local is_array = (#value == n)
  if is_array then
    local parts = {}
    for i = 1, #value do
      parts[i] = pad_n .. M.pretty_json(value[i], indent + 1)
    end
    return "[\n" .. table.concat(parts, ",\n") .. "\n" .. pad .. "]"
  end
  local keys = {}
  for k, _ in pairs(value) do
    if type(k) == "string" then keys[#keys + 1] = k end
  end
  table.sort(keys)
  local parts = {}
  for _, k in ipairs(keys) do
    parts[#parts + 1] = string.format("%s%q: %s",
      pad_n, k, M.pretty_json(value[k], indent + 1))
  end
  return "{\n" .. table.concat(parts, ",\n") .. "\n" .. pad .. "}"
end

function M.humanize_duration_ms(ms)
  if ms == nil then return nil end
  if ms < 1000 then return tostring(math.floor(ms)) .. "ms" end
  if ms < 60000 then return string.format("%ds", math.floor(ms / 1000)) end
  local m = math.floor(ms / 60000)
  local s = math.floor((ms % 60000) / 1000)
  return string.format("%dm%02ds", m, s)
end

function M.humanize_tokens(n)
  if n == nil then return nil end
  if n < 1000 then return tostring(n) end
  if n < 1000000 then return tostring(math.floor(n / 1000)) .. "k" end
  return string.format("%.1fM", n / 1000000)
end

-- Pad every line of `text` with trailing spaces to the longest line's
-- width so a styled bg renders as a rectangle instead of ragging out
-- to per-line content widths. Callers render with `wrap = "none"`
-- when this matters; padding only fixes natural-line raggedness, not
-- post-wrap raggedness.
function M.pad_block(text)
  if type(text) ~= "string" or #text == 0 then return text end
  local lines = {}
  for line in text:gmatch("([^\n]*)") do
    lines[#lines + 1] = line
  end
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
--    or chain another tool call. The real result will arrive later as
--    a user message tagged `[spawn_graph(run_id=<id>) result]`."
-- That whole blob is LLM instruction noise. Surface just the run_id —
-- progress is already visible in the DAG sidebar.
function M.format_spawn_graph_output(output)
  if type(output) ~= "string" or #output == 0 then return output end
  local run_id = output:match("run_id=([%w%-]+)")
  if run_id then return "submitted as " .. run_id end
  return output
end

-- Render a `spawn_graph` args.graph as a compact node-list + edge-list.
-- Args of each node are deliberately omitted so the popup stays
-- scannable; a future "focus a node" UI can surface them on demand.
function M.format_graph(graph)
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

-- Render a `dispatch-graph` args table (`{ nodes = [{ id, role,
-- dependencies }] }`) as a yaml-like two-section block: `nodes:` lists
-- ids with arrow-notated dependencies, `agents:` maps each id to its
-- assigned role. Two sections instead of one merged line so the user
-- can scan topology and role assignment independently — same shape the
-- approve popup used before the args-passthrough refactor.
function M.format_dispatch_graph(nodes)
  if type(nodes) ~= "table" or #nodes == 0 then return "(empty graph)" end
  local lines = { "nodes:" }
  for _, n in ipairs(nodes) do
    local id = n.id or "?"
    local deps = n.dependencies
    if type(deps) == "table" and #deps > 0 then
      lines[#lines + 1] = "  " .. id .. " -> " .. table.concat(deps, ", ")
    else
      lines[#lines + 1] = "  " .. id
    end
  end
  lines[#lines + 1] = ""
  lines[#lines + 1] = "agents:"
  for _, n in ipairs(nodes) do
    local id = n.id or "?"
    local role = n.role or "?"
    lines[#lines + 1] = "  " .. id .. ": " .. role
  end
  return table.concat(lines, "\n")
end

-- Pretty-print an args table from a `chat.tool.popup_request` event
-- so the popup body shows a human-legible summary of the call.
-- Stringy values render verbatim; nested tables get a compact `{...}`
-- placeholder rather than a recursive dump (most tools take flat args,
-- and a long nested blob would blow up the popup anyway). The
-- `spawn_graph` and `dispatch-graph` tools get dedicated layouts.
function M.format_args(args)
  if args == nil then return "" end
  if type(args) ~= "table" then return tostring(args) end
  if type(args.graph) == "table" then
    return M.format_graph(args.graph)
  end
  -- dispatch-graph shape: top-level `nodes` array whose entries carry
  -- `role` (lead-workflow's role-aware spec). Detect via the role field
  -- on the first entry to avoid colliding with any future tool that
  -- also names its arg `nodes` but with a different shape.
  if type(args.nodes) == "table" and type(args.nodes[1]) == "table"
      and args.nodes[1].role ~= nil then
    return M.format_dispatch_graph(args.nodes)
  end
  local keys = {}
  for k, _ in pairs(args) do
    if type(k) == "string" then keys[#keys + 1] = k end
  end
  table.sort(keys)
  if #keys == 0 then
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

M.C = {
  user            = "#7FB4FF",
  system          = "#808080",
  status_dim      = "#606060",
  status_warn     = "#D7AF5F",
  status_danger   = "#D75F5F",
  status_ok       = "#87D787",
  md_heading      = "#FFB86C",
  md_code_fg      = "#C0C0C0",
  -- Code-block bg: a clearly-grey rectangle that stays readable
  -- against `md_code_fg` on every terminal profile (~7:1 contrast).
  -- Darker values muddy out on warm-colour profiles.
  md_code_inline_bg = "#3a3a3a",
  md_code_block_bg  = "#3a3a3a",
  footer          = "#707070",
  -- Plan-message border. Yellow distinguishes a write-review plan from
  -- the user's blue block and the system's grey-italic line. Bright
  -- gold reads on every terminal profile and doesn't collide with the
  -- existing status_warn (#D7AF5F) which carries semantic "warning"
  -- weight; plans aren't warnings, they're a third entry kind.
  plan            = "#FFD75F",
  -- graph_result header colour. Cyan picks up the orchestration /
  -- sub-graph register — distinct from tool_name (md_heading orange,
  -- request side) and plan (yellow, write-review). The blockquote
  -- accent (#7faaaa) is the closest existing neighbour but reads as
  -- "quoted text"; a brighter saturated cyan separates the graph-result
  -- block from prose without colliding with status_warn / plan yellow.
  graph_result    = "#5FD7D7",
}

local C = M.C

M.STYLE = {
  user_chrome     = { fg = C.user, bold = true },
  user_chrome_queued = { fg = C.status_dim, bold = false },
  -- Input-field border. Same blue as user blocks so the input reads as
  -- a peer to user message blocks rather than a separate widget kind.
  input_border          = { fg = C.user, bold = true },
  input_border_unfocused= { fg = C.status_dim },
  body_default    = nil,
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
  graph_result_name   = { fg = C.graph_result, bold = true },
  graph_result_error  = { fg = C.status_danger, bold = true },
  popup_user      = { fg = C.user, bold = true },
  popup_warn      = { fg = C.status_warn, bold = true },
  popup_danger    = { fg = C.status_danger, bold = true },
  popup_info      = { fg = C.user, bold = true },
  toast           = { fg = C.user },
  dag_separator   = { fg = C.footer },
  dag_pending     = { fg = C.status_dim },
  dag_running     = { fg = C.status_warn },
  dag_done        = { fg = C.status_ok },
  dag_error       = { fg = C.status_danger, bold = true },
  dag_skipped     = { fg = C.status_dim, italic = true },
  -- Plan-entry chrome (yellow border). `bold = true` matches
  -- user_chrome's weight so the two read as parallel "input/output of
  -- a decision" frames at equal visual weight.
  plan_chrome           = { fg = C.plan, bold = true },
  -- Approved plans dim the border so the chat scroll de-emphasises
  -- already-resolved plans without hiding them. Rejected plans go
  -- danger-red so they stand out as "do NOT proceed on this".
  plan_chrome_approved  = { fg = C.plan, italic = true },
  plan_chrome_rejected  = { fg = C.status_danger, strikethrough = true },
  plan_subtitle         = { fg = C.footer },
  plan_hint             = { fg = C.footer, italic = true },
  plan_status_approved  = { fg = C.status_ok, bold = true },
  plan_status_rejected  = { fg = C.status_danger, bold = true },
}

M.MARKDOWN_THEME = {
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

M.CURSOR_ROW_STYLE = { fg = "#000000", bg = C.user }

function M.md(source)
  return tui.markdown { source = source or "", theme = M.MARKDOWN_THEME, wrap = "word" }
end

return M
