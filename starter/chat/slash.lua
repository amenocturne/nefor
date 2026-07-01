-- Slash command registry + prompt-widget completion declarations.
-- chat.lua passes `M.completions()` to the prompt widget; the widget
-- owns trigger detection, popup rendering, Tab/Esc routing. This
-- module owns the data (slash list, @-path filesystem source).

local M = {}

M.COMMANDS = {
  { name = "new",     aliases = { "clear" }, hint = "start a fresh chat (clears transcript)", takes_args = false },
  { name = "help",    aliases = {},          hint = "show the help popup",                    takes_args = false },
  { name = "quit",    aliases = { "exit" },  hint = "exit nefor",                             takes_args = false },
  { name = "login",   aliases = {},          hint = "authenticate a provider",                takes_args = true },
  { name = "logout",  aliases = {},          hint = "revoke a provider's auth",               takes_args = true },
  { name = "model",   aliases = {},          hint = "list/switch active model",               takes_args = true },
  {
    name = "mode", aliases = {}, hint = "switch workflow mode", takes_args = true,
    arg_completions = {
      { name = "default", hint = "normal agentic workflow" },
    },
  },
  { name = "think",   aliases = { "effort" }, hint = "set reasoning effort",                  takes_args = true },
  { name = "compact", aliases = {},          hint = "compact active model context",           takes_args = false },
  { name = "resume",  aliases = {},          hint = "resume previous session",                takes_args = true },
  { name = "safe",    aliases = {},          hint = "use safe permission prompts",           takes_args = false },
  { name = "auto",    aliases = {},          hint = "auto-deny requests needing humans",      takes_args = false },
  { name = "yolo",    aliases = {},          hint = "approve all tool requests (DANGEROUS)",  takes_args = false },
  { name = "approve", aliases = {},          hint = "approve the pending plan (optional reason)", takes_args = true },
  { name = "reject",  aliases = {},          hint = "reject the pending plan with a reason", takes_args = true },
  { name = "debug",   aliases = {},          hint = "toggle diagnostic logging",             takes_args = false },
}

local function command_matches(cmd, q)
  if cmd.name:lower():sub(1, #q) == q then return true end
  for _, a in ipairs(cmd.aliases or {}) do
    if a:lower():sub(1, #q) == q then return true end
  end
  return false
end

local function command_by_exact_name_or_alias(q)
  for _, cmd in ipairs(M.COMMANDS) do
    if cmd.name:lower() == q then return cmd end
    for _, a in ipairs(cmd.aliases or {}) do
      if a:lower() == q then return cmd end
    end
  end
  return nil
end

local function ranked_command_matches(q)
  local ranked = {}
  for idx, cmd in ipairs(M.COMMANDS) do
    if command_matches(cmd, q) then ranked[#ranked + 1] = { entry = cmd, order = idx } end
  end
  table.sort(ranked, function(a, b)
    local an = a.entry.name:lower()
    local bn = b.entry.name:lower()
    local a_exact = an == q
    local b_exact = bn == q
    if a_exact ~= b_exact then return a_exact end
    if #an ~= #bn then return #an < #bn end
    return a.order < b.order
  end)
  local out = {}
  for _, item in ipairs(ranked) do out[#out + 1] = item.entry end
  return out
end

local function slash_arg_filter(query)
  local q = (query or ""):lower()
  local cmd_name, arg_query = q:match("^(%S+)%s+(.*)$")
  if cmd_name == nil then return nil end

  local cmd = command_by_exact_name_or_alias(cmd_name)
  if cmd == nil or type(cmd.arg_completions) ~= "table" then return {} end

  local out = {}
  for _, arg in ipairs(cmd.arg_completions) do
    local arg_name = tostring(arg.name or "")
    if arg_name:lower():sub(1, #arg_query) == arg_query then
      out[#out + 1] = {
        name = cmd.name .. " " .. arg_name,
        hint = arg.hint,
        takes_args = false,
      }
    end
  end
  return out
end

local function slash_filter(query)
  local arg_matches = slash_arg_filter(query)
  if arg_matches ~= nil then return arg_matches end

  -- Case-insensitive prefix match against name OR aliases.
  return ranked_command_matches((query or ""):lower())
end

-- Parse `/cmd args` out of an input value. Returns (cmd, args, has_ws).
-- `cmd` nil when text isn't a slash command.
function M.parse(text)
  if text:sub(1, 1) ~= "/" then return nil, nil, false end
  local cmd, rest = text:match("^/(%S+)%s*(.*)$")
  -- Absolute filesystem paths also start with `/`. Treat `/Users/a.png`
  -- or `/tmp/paste.png` as plain chat text so clipboard-image paths can
  -- be submitted directly instead of becoming an unknown slash command,
  -- but only after extracting the command word so known slash commands
  -- like `/safe`, `/auto`, and `/yolo` still work.
  if text:find("^/[^%s]+/") then
    local known = false
    for _, entry in ipairs(M.COMMANDS) do
      if entry.name == cmd then known = true; break end
    end
    if not known then return nil, nil, false end
  end
  local has_ws = text:find("^/%S+%s") ~= nil
  return cmd, (rest ~= "" and rest or nil), has_ws
end

-- @-path filesystem source.
-- The user types `@<path>` mid-message; the prompt widget routes
-- per-keystroke trigger detection + Tab application, this module
-- supplies the directory listing + filter:
--
--   * walked one directory level at a time, bash-style — `@src/m`
--     lists `src/` entries whose name starts with `m`, NOT a recursive
--     walk from CWD;
--   * excludes hidden files (leading `.`) and the IGNORE allowlist;
--   * dirs-first then case-insensitive alphabetical so drill-down
--     candidates lead the popup;
--   * cached per base_dir under a module-local closure so repeated
--     keystrokes within the same dir (or back-stepping over previously
--     visited dirs) reuse one readdir.

local AT_COMPLETION_CAP = 200

local AT_COMPLETION_IGNORE = {
  [".git"] = true, ["node_modules"] = true,
  ["target"] = true, ["__pycache__"] = true,
}

-- Split `body` (the part after `@`) into (base_dir, leaf). base_dir
-- is everything up to and including the last `/`; leaf is the prefix
-- filter against directory entries. Trailing `/` means leaf = "".
local function split_body(body)
  if body == nil then return "", "" end
  local last_slash = body:find("/[^/]*$")
  if last_slash == nil then
    return "", body
  end
  return body:sub(1, last_slash), body:sub(last_slash + 1)
end

-- Resolve `base_dir` against CWD. Empty → CWD. Paths starting with
-- `/` are treated as absolute.
local function resolve_base_dir(base_dir)
  if base_dir == nil or base_dir == "" then return "." end
  if base_dir:sub(1, 1) == "/" then return base_dir end
  return "./" .. base_dir
end

local function ls_entries(dir)
  local entries, err = nefor.fs.list_dir(dir)
  if entries == nil then
    -- Half-typed dir, permission denied, etc. Silently return empty
    -- so the popup shows "no matches" rather than raising.
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
    if a.is_dir ~= b.is_dir then return a.is_dir end
    return a.name:lower() < b.name:lower()
  end)
  return out
end

local dir_cache = {}

local function at_source(body)
  local base_dir = split_body(body)
  local cached = dir_cache[base_dir]
  if cached == nil then
    cached = ls_entries(resolve_base_dir(base_dir))
    dir_cache[base_dir] = cached
  end
  return cached
end

local function at_filter(entries, body)
  local _, leaf = split_body(body)
  local q = (leaf or ""):lower()
  if q == "" then return entries end
  local out = {}
  for _, e in ipairs(entries) do
    if e.name:lower():sub(1, #q) == q then out[#out + 1] = e end
  end
  return out
end

local function at_format(entry)
  -- Trailing `/` on directories so the user sees at a glance which
  -- entries are drillable.
  return entry.is_dir and (entry.name .. "/") or entry.name
end

-- Replace the trailing `@<token>` with `@<base_dir><name>(/ if dir)`.
-- The prompt widget calls this with the apply contract
-- (entry, body, value, token); we walk back from the value's end to
-- find the token position rather than re-scanning, since the widget
-- already validated the token shape.
local function at_apply(entry, body, value, token)
  if value == nil or token == nil then return value end
  local pos = #value - #token + 1
  if pos < 1 then return value end
  local base_dir = split_body(body or "")
  local replacement = "@" .. base_dir .. entry.name
  if entry.is_dir then replacement = replacement .. "/" end
  if replacement:find("%s") then
    replacement = "@\"" .. replacement:sub(2):gsub('"', '\\"') .. "\""
  end
  return value:sub(1, pos - 1) .. replacement
end

local function slash_format(cmd)
  return string.format("/%-16s  %s", cmd.name, cmd.hint or "")
end

local function slash_apply(entry)
  return "/" .. entry.name .. (entry.takes_args and " " or "")
end

-- Completion sources for the prompt widget. Each entry carries:
--   trigger      — first char (`/` or `@`).
--   anchor       — "start" fires only at column 0 (slash command);
--                  "word" fires at end-of-word (path ref mid-message).
--   source       — entry list. Called with the current trigger body
--                  so @-path can pick a base_dir per keystroke.
--   filter       — fn(entries, body) → filtered list. Default is
--                  leaf-prefix; slash also matches aliases.
--   format_entry — fn(entry) → display string (no styling).
--   apply        — fn(entry, body, value, token) → new input value.
function M.completions()
  return {
    {
      trigger      = "/",
      anchor       = "start",
      source       = function() return M.COMMANDS end,
      filter       = function(_, body) return slash_filter(body or "") end,
      format_entry = slash_format,
      apply        = slash_apply,
    },
    {
      trigger      = "/",
      anchor       = "start-spaced",
      source       = function() return M.COMMANDS end,
      filter       = function(_, body) return slash_filter(body or "") end,
      format_entry = slash_format,
      apply        = slash_apply,
    },
    {
      trigger      = "@",
      anchor       = "word",
      source       = at_source,
      filter       = at_filter,
      format_entry = at_format,
      apply        = at_apply,
    },
  }
end

return M
