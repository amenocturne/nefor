-- Dump-to-file layer for huge tool results. When a tool.result's
-- output exceeds the inline budget, the full payload lands on disk
-- under `<data_root>/tool-results/<chat_id>/<call_id>.txt` and only
-- a summary + path reference flows into the model's chat history.
-- The model greps the file from a later turn (via bash) to extract
-- specifics it didn't get in the inlined preview.

local json = nefor.json

local M = {}

-- 32 KiB comfortably fits typical read_file / ls / short grep while
-- catching the cases that genuinely blow up model context (10k-line
-- files, recursive grep across a monorepo).
M.INLINE_BUDGET = 32 * 1024

-- 4 KiB head-preview alongside the "written to <path>" notice — enough
-- for the model to recognise what it's looking at and decide on a grep
-- pattern, without replicating the whole thing back into context.
M.PREVIEW_BYTES = 4 * 1024

-- Delegates to `nefor.fs.data_root()` — the engine's canonical resolved
-- data directory (CLI flag > NEFOR_DATA_DIR env var > XDG default).
-- This module runs in the engine Lua VM (tool-gate's wrapper actor),
-- so the binding is always available.
---@return string|nil
local function data_root()
  return nefor.fs.data_root()
end

---@param path string
local function ensure_dir(path)
  -- Best-effort recursive mkdir via the Rust binding. Idempotent on
  -- EEXIST and surfaces permission errors on the subsequent io.open
  -- with a real error string (the return value here is intentionally
  -- ignored — the next write_file is the source of truth on success).
  nefor.fs.mkdir_p(path)
end

-- Realistic chat_ids are already filesystem-safe (UUIDs, `chat-1`);
-- this rules out path traversal from a malformed value escaping the
-- tool-results root.
---@param scope string|nil
---@return string
local function safe_scope(scope)
  if type(scope) ~= "string" or scope == "" then return "_unscoped" end
  local cleaned = scope:gsub("[^%w%-_]", "_")
  if cleaned == "" then return "_unscoped" end
  return cleaned
end

---@param call_id string|nil
---@return string|nil
local function safe_call_id(call_id)
  if type(call_id) ~= "string" or call_id == "" then return nil end
  local cleaned = call_id:gsub("[^%w%-_]", "_")
  if cleaned == "" then return nil end
  return cleaned
end

-- Strings pass through; anything else is JSON-encoded so a
-- table-shaped output gets a deterministic textual form for both the
-- size check and the on-disk write (the model greps the textual form).
---@param output any
---@return string|nil, string|nil
local function stringify(output)
  if type(output) == "string" then return output, nil end
  if output == nil then return "", nil end
  local ok, encoded = pcall(json.encode, output)
  if not ok then
    return nil, "json.encode: " .. tostring(encoded)
  end
  return encoded, nil
end

---@param value any
---@return boolean
local function is_image_media_table(value)
  return type(value) == "table"
      and value.type == "media"
      and type(value.media_type) == "string"
      and value.media_type:match("^image/") ~= nil
end

---@param output any
---@return table|nil
local function image_media_output(output)
  if is_image_media_table(output) then return output end
  if type(output) ~= "string" then return nil end

  local ok, decoded = pcall(json.decode, output)
  if not ok then return nil end
  if is_image_media_table(decoded) then return decoded end
  return nil
end

---@param output any
---@return boolean
function M.is_image_media_output(output)
  return image_media_output(output) ~= nil
end

---@param output any
---@return string|nil
function M.image_media_summary(output)
  local media = image_media_output(output)
  if not media then return nil end
  local filename = media.filename
  if type(filename) ~= "string" or filename == "" then
    filename = "image"
  end
  return string.format("[image result: %s (%s)]", filename, media.media_type)
end

---@param b integer|nil
---@return boolean
local function is_continuation(b)
  return type(b) == "number" and b >= 0x80 and b <= 0xBF
end

---@param b integer|nil
---@return boolean
local function is_continuation_after(first, b)
  if not is_continuation(b) then return false end
  if first == 0xE0 then return b >= 0xA0 end
  if first == 0xED then return b <= 0x9F end
  if first == 0xF0 then return b >= 0x90 end
  if first == 0xF4 then return b <= 0x8F end
  return true
end

---@param payload string
---@param max_bytes integer
---@return string
local function utf8_preview(payload, max_bytes)
  local len = #payload
  local out = {}
  local out_len = 0
  local i = 1

  while i <= len and out_len < max_bytes do
    local b1 = payload:byte(i)
    local seq_len = 0

    if b1 < 0x80 then
      seq_len = 1
    elseif b1 >= 0xC2 and b1 <= 0xDF and is_continuation(payload:byte(i + 1)) then
      seq_len = 2
    elseif b1 >= 0xE0 and b1 <= 0xEF
        and is_continuation_after(b1, payload:byte(i + 1))
        and is_continuation(payload:byte(i + 2)) then
      seq_len = 3
    elseif b1 >= 0xF0 and b1 <= 0xF4
        and is_continuation_after(b1, payload:byte(i + 1))
        and is_continuation(payload:byte(i + 2))
        and is_continuation(payload:byte(i + 3)) then
      seq_len = 4
    end

    if seq_len > 0 then
      if out_len + seq_len > max_bytes then break end
      out[#out + 1] = payload:sub(i, i + seq_len - 1)
      out_len = out_len + seq_len
      i = i + seq_len
    else
      out[#out + 1] = "?"
      out_len = out_len + 1
      i = i + 1
    end
  end

  return table.concat(out)
end

-- Cheap short-circuit for the common case (small output → no work).
---@param output any
---@return boolean
function M.should_dump(output)
  if M.is_image_media_output(output) then
    return false
  end
  local s = stringify(output)
  if not s then return false end
  return #s > M.INLINE_BUDGET
end

---@param payload string  -- already stringified
---@param path string
---@return string
local function summarise(payload, path)
  local total = #payload
  local preview = utf8_preview(payload, math.min(M.PREVIEW_BYTES, total))
  return table.concat({
    "[Output written to " .. path .. "; " .. tostring(total)
      .. " bytes; truncated preview below]",
    "",
    preview,
    "",
    "... [output continues; full content at " .. path
      .. "; use search_text/grep on the path, or read_file with offset + "
      .. "max_bytes to read bounded chunks] ...",
  }, "\n")
end

---@param path string
---@param contents string
---@return boolean, string|nil
local function write_file(path, contents)
  local fh, err = io.open(path, "w")
  if not fh then return false, tostring(err) end
  local ok, write_err = pcall(function() fh:write(contents) end)
  fh:close()
  if not ok then return false, tostring(write_err) end
  return true, nil
end

-- Best-effort: meta is debugging surface, not load-bearing.
---@param meta_path string
---@param meta table
local function write_meta(meta_path, meta)
  local ok, encoded = pcall(json.encode, meta)
  if not ok then return end
  pcall(function() write_file(meta_path, encoded) end)
end

-- Write the full output to disk and return summary + path. On disk
-- error returns `nil, nil, err` so the caller can degrade to the
-- un-replaced output.
---@param chat_id string|nil
---@param call_id string
---@param output any
---@param args table|nil  -- { tool?, args? } — optional metadata for the meta companion
---@return string|nil summary
---@return string|nil path
---@return string|nil err
function M.dump(chat_id, call_id, output, args)
  local payload, encode_err = stringify(output)
  if not payload then return nil, nil, encode_err end

  local root = data_root()
  if not root then
    return nil, nil, "no data root (nefor.fs.data_root unavailable)"
  end

  local cid = safe_call_id(call_id)
  if not cid then return nil, nil, "missing call_id" end

  local scope_dir = root .. "/tool-results/" .. safe_scope(chat_id)
  ensure_dir(scope_dir)

  local path = scope_dir .. "/" .. cid .. ".txt"
  local ok, err = write_file(path, payload)
  if not ok then return nil, nil, err end

  if type(args) == "table" then
    write_meta(scope_dir .. "/" .. cid .. ".meta.json", {
      tool        = args.tool,
      args        = args.args,
      total_bytes = #payload,
      timestamp   = nefor.engine.now(),
    })
  end

  return summarise(payload, path), path, nil
end

-- Test-only: expose internals.
M._stringify  = stringify
M._summarise  = summarise
M._safe_scope = safe_scope
M._data_root  = data_root
M._utf8_preview = utf8_preview

return M
