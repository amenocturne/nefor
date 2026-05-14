-- Session-picker data layer. `/resume` (no args) pops a picker showing
-- the last N sessions on disk newest-first with a one-line preview of
-- the first user prompt. Selecting a row emits a
-- `sessions.resume_request { session_id }` envelope onto the NCP bus;
-- the starter's `sessions` Lua module subscribes to that kind and runs
-- the in-process swap (emit session_end → swap state → session_start →
-- replay jsonl → resume_done). No process exit, no sidechannel file —
-- the TUI stays alive across the whole flip.

local common = require("chat.common")

local M = {}

function M.session_dir()
  local root = common.data_root()
  if root == nil then return nil end
  return root .. "/sessions"
end

-- Extract `text` from a session-log JSONL line carrying a
-- chat.input.submit event. The wire shape is:
--   {"ts":"...","origin":"...","payload":"{\"type\":\"event\",\"body\":{\"kind\":\"chat.input.submit\",\"text\":\"<actual>\"}}"}
-- The text field lives inside the embedded JSON string of `payload`,
-- so the literal JSONL bytes contain `\"text\":\"<value>\"` (each
-- quote backslash-escaped once). We avoid pulling in a full JSON
-- parser because the TUI Lua VM doesn't expose nefor.json, and the
-- picker is dev tooling — a regex-tier extraction is fine.
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
            out[#out + 1] = '"';  i = i + 3
          elseif nnxt == "\\" then
            out[#out + 1] = "\\"; i = i + 4
          elseif nnxt == "n" then
            out[#out + 1] = "\n"; i = i + 3
          elseif nnxt == "t" then
            out[#out + 1] = "\t"; i = i + 3
          else
            out[#out + 1] = nnxt; i = i + 3
          end
        else
          out[#out + 1] = nxt; i = i + 2
        end
      else
        -- Singly-encoded: standard JSON string escapes.
        if nxt == '"' then
          out[#out + 1] = '"';  i = i + 2
        elseif nxt == "\\" then
          out[#out + 1] = "\\"; i = i + 2
        elseif nxt == "n" then
          out[#out + 1] = "\n"; i = i + 2
        elseif nxt == "t" then
          out[#out + 1] = "\t"; i = i + 2
        else
          out[#out + 1] = nxt; i = i + 2
        end
      end
    elseif c == '"' and not doubly_encoded then
      return table.concat(out)
    else
      out[#out + 1] = c; i = i + 1
    end
  end
  return nil
end

-- Extract started_at from the JSONL header. We don't need a full JSON
-- parser — pattern-match `"started_at":"<value>"`.
local function extract_started_at(header_line)
  return header_line:match('"started_at"%s*:%s*"([^"]+)"')
end

-- List up to `limit` newest sessions on disk. Each row:
--   { id, path, started_at, preview }
-- Preview is best-effort: scan the JSONL for the first
-- `chat.input.submit` event and pull `text`. Sessions with no submits
-- (e.g. crashed boots) get a "(no submits)" placeholder. started_at
-- comes from the header — falls back to "?" if the header is missing
-- or malformed.
function M.list_recent(limit)
  local dir = M.session_dir()
  if dir == nil then return {} end
  -- `ls -t` sorts newest mtime first. Pure-Lua dir iteration would
  -- need LuaFileSystem; io.popen is enabled by mlua's safe stdlib.
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

-- Truncate `text` to `n` columns (byte-count proxy; non-ASCII
-- previews may render slightly off but the picker is dev-tooling).
-- Newlines collapse to spaces so multi-line prompts render as a
-- single row.
function M.clip_preview(text, n)
  if text == nil then return "" end
  text = tostring(text):gsub("\n", " "):gsub("\r", " ")
  if #text <= n then return text end
  return text:sub(1, math.max(0, n - 1)) .. "…"
end

-- Format the started_at timestamp for picker rows. ISO 8601 →
-- "MM-DD HH:MM". Falls back to the raw string on parse failure.
function M.format_started_at(s)
  if type(s) ~= "string" then return "?" end
  local mo, dy, hh, mm = s:match("^%d%d%d%d%-(%d%d)%-(%d%d)T(%d%d):(%d%d)")
  if mo == nil then return s end
  return string.format("%s-%s %s:%s", mo, dy, hh, mm)
end

-- Build a `sessions.resume_request` send_to effect for `id`. The
-- starter's sessions module subscribes to this kind and runs the swap
-- sequence in-process. No process exit, no file write — the TUI stays
-- alive across the resume.
function M.emit_resume_request(id)
  return {
    kind   = "send_to",
    target = "engine",
    body   = { kind = "sessions.resume_request", session_id = id },
  }
end

return M
