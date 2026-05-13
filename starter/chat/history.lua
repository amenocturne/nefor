-- Shell-style persistent input history. Submitted prompts (NOT every
-- keystroke — only the user-typed text at submit time) live in
-- `state.prompt_history` in-memory and mirror to a single file on disk
-- so they survive nefor restarts. Past INPUT_HISTORY_MAX entries the
-- oldest roll off the disk file the next time we trim.

local M = {}

M.INPUT_HISTORY_MAX = 50

-- Same env-var precedence as the engine's nefor.fs.data_root()
-- (NEFOR_DATA_DIR → XDG_DATA_HOME/nefor → HOME/.local/share/nefor) so
-- input-history sits next to session jsonls under the same root.
-- This module runs in the TUI plugin's subprocess VM where the engine
-- `nefor.fs.*` bindings aren't installed; the resolver here is a
-- matching reimplementation rather than a delegation.
local function data_root()
  local override = os.getenv("NEFOR_DATA_DIR")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function history_path()
  local root = data_root()
  if root == nil then return nil end
  return root .. "/input-history"
end

-- One-line escaping: `\` → `\\`, real `\n` → `\n` literal (two chars),
-- real `\r` → `\r`. Decode reverses. Together this guarantees every
-- entry fits on a single physical line regardless of newlines / tabs /
-- backslashes the user pasted. Cheaper than pulling in a JSON parser
-- (the TUI Lua VM doesn't expose nefor.json) and the format is
-- self-describing — a developer reading the file sees the obvious
-- shape.
local function encode_line(text)
  if text == nil then return "" end
  local s = tostring(text)
  s = s:gsub("\\", "\\\\")
  s = s:gsub("\n", "\\n")
  s = s:gsub("\r", "\\r")
  return s
end

local function decode_line(line)
  if line == nil then return nil end
  local out = {}
  local i = 1
  local n = #line
  while i <= n do
    local c = line:sub(i, i)
    if c == "\\" and i < n then
      local nxt = line:sub(i + 1, i + 1)
      if nxt == "n" then
        out[#out + 1] = "\n"; i = i + 2
      elseif nxt == "r" then
        out[#out + 1] = "\r"; i = i + 2
      elseif nxt == "\\" then
        out[#out + 1] = "\\"; i = i + 2
      else
        -- Unknown escape — keep verbatim. Future-proof against new
        -- escape kinds added by readers without breaking existing
        -- files.
        out[#out + 1] = c; i = i + 1
      end
    else
      out[#out + 1] = c; i = i + 1
    end
  end
  return table.concat(out)
end

-- Best-effort `mkdir -p` so the writer can drop the file even on a
-- fresh data-root. Lua doesn't ship an in-process mkdir; shell out.
-- Errors here mean the writer's io.open will fail next, which the
-- writer logs and swallows — history just won't persist this session.
local function ensure_dir()
  local root = data_root()
  if root == nil then return end
  os.execute(string.format("mkdir -p %q 2>/dev/null", root))
end

-- Read the on-disk history into the in-memory list shape the rest of
-- chat.lua uses: newest at index 1. The file is written newest-first
-- on every append, so a forward read into a list keeps that order.
-- Caps at INPUT_HISTORY_MAX defensively in case an older nefor wrote
-- beyond the current cap.
function M.load()
  local path = history_path()
  if path == nil then return {} end
  local f = io.open(path, "r")
  if f == nil then return {} end
  local out = {}
  for line in f:lines() do
    if #line > 0 then
      out[#out + 1] = decode_line(line)
      if #out >= M.INPUT_HISTORY_MAX then break end
    end
  end
  f:close()
  return out
end

-- Persist a `history` list. Called after every submit; rewrites the
-- whole file rather than appending + truncating because the file is
-- small (≤ INPUT_HISTORY_MAX lines) and the rewrite is atomic enough
-- for our durability needs (last-session crash loses at most the tail
-- entry — os.rename-style atomic-replace via tmp file is overkill for
-- shell-history-grade data). I/O failure is best-effort.
function M.persist(history)
  local path = history_path()
  if path == nil then return end
  ensure_dir()
  local f = io.open(path, "w")
  if f == nil then return end
  for i = 1, math.min(#history, M.INPUT_HISTORY_MAX) do
    f:write(encode_line(history[i]))
    f:write("\n")
  end
  f:close()
end

return M
