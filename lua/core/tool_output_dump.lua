-- starter/lib/tool_output_dump.lua — dump-to-file layer for huge tool
-- results. Per the lead-workflow spec §5: when a tool call returns a
-- payload past a threshold (large `read_file`, big `grep`, deep `find`,
-- etc.), the **full** result lands on disk at a persistent path that
-- outlives nefor's runtime, and only a synthesised summary + path
-- reference flows on into the model's chat history. The model can grep
-- the dumped file from a subsequent turn (via the bash tool) to extract
-- specifics it didn't get in the inlined preview.
--
-- This module is the pure logic of that swap: size check, file write,
-- summary string. The interception point — where to call `dump()` for
-- inbound `tool.result` envelopes — lives in `starter/tool-gate/init.lua`,
-- which is the canonical wrapper sitting between tool plugins and the
-- agentic-loop. Doing the swap there catches every tool's output
-- uniformly without each tool plugin needing to opt in.
--
-- ## Storage layout
--
-- Files land under `<NEFOR_DATA_HOME>/tool-results/<chat_id>/<call_id>.txt`
-- mirroring `starter/sessions/init.lua`'s data-root resolution
-- (`NEFOR_DATA_HOME` → `XDG_DATA_HOME/nefor` → `$HOME/.local/share/nefor`).
-- A sibling `<call_id>.meta.json` records `{ tool, args, total_bytes,
-- timestamp }` for debugging — small, optional, ignorable.
-- `chat_id` is best-effort; when the caller can't surface one (early
-- spawn-graph firings, sub-graphs whose chat scoping isn't exposed yet)
-- the file falls under `_unscoped/`.
--
-- ## Replacement shape
--
-- `dump()` returns a synthesised payload that the wrapper splices in
-- place of `body.output` on the inbound `tool.result`:
--
--   [Output written to <path>; <total_bytes> bytes; truncated preview below]
--
--   <first 4 KiB of original output>
--
--   ... [output continues; full content at <path>; use `grep <pattern>
--   <path>` or `head -n <N> <path>` to extract more] ...
--
-- The model sees this string. The full content is on disk for it to
-- grep with the bash tool when it needs more.
--
-- ## Failure mode
--
-- Disk-write failures degrade gracefully: `dump()` returns `nil, err`,
-- the wrapper logs a warning and forwards the original `tool.result`
-- unchanged. Better to ship the un-replaced (and possibly large) output
-- than to drop it on the floor.

local json = nefor.json

local M = {}

-- Inline budget for the body.output field (bytes, post-serialisation).
-- Anything past this gets dumped to disk + summarised. Tunable; v1
-- starts at 32 KiB which comfortably fits most tool calls (read_file
-- against a typical source file, ls of a normal directory, short grep)
-- while catching the cases that genuinely blow up the model context
-- (10k-line files, recursive grep across a monorepo).
M.INLINE_BUDGET = 32 * 1024

-- Bytes of original output that get inlined as a preview alongside the
-- "written to <path>" notice. Picked to give the model enough surface
-- to recognise what it's looking at + decide on a grep pattern, without
-- replicating the whole thing back into context.
M.PREVIEW_BYTES = 4 * 1024

-- ------------------------------------------------------------------
-- data-root resolution — same precedence as starter/sessions/init.lua
-- so a single env var (NEFOR_DATA_HOME) controls both. The order is:
--   1. $NEFOR_DATA_HOME   (test override + canonical setting)
--   2. $XDG_DATA_HOME/nefor
--   3. $HOME/.local/share/nefor
-- ------------------------------------------------------------------

---@return string|nil
local function data_root()
  local override = os.getenv("NEFOR_DATA_HOME")
  if override and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME")
  if not home or home == "" then return nil end
  return home .. "/.local/share/nefor"
end

---@param path string
local function ensure_dir(path)
  -- Best-effort recursive mkdir. The `2>/dev/null` swallows EEXIST and
  -- permission errors — they surface again on the subsequent io.open
  -- when we actually try to write the file, with a real error string.
  os.execute(string.format("mkdir -p %q 2>/dev/null", path))
end

-- Sanitise a chat_id-shaped scope segment for use as a directory name.
-- Realistic chat_ids are already filesystem-safe (`chat-1`, UUIDs), but
-- a malformed value shouldn't be able to escape the tool-results root.
---@param scope string|nil
---@return string
local function safe_scope(scope)
  if type(scope) ~= "string" or scope == "" then return "_unscoped" end
  -- Strip anything that isn't an alphanumeric, dash, or underscore.
  -- That's stricter than POSIX needs but covers every real chat_id
  -- shape we mint and rules out path traversal.
  local cleaned = scope:gsub("[^%w%-_]", "_")
  if cleaned == "" then return "_unscoped" end
  return cleaned
end

-- Same for call_id — used as the leaf filename so we don't want a
-- caller-supplied slash to land us in an unexpected directory.
---@param call_id string|nil
---@return string|nil
local function safe_call_id(call_id)
  if type(call_id) ~= "string" or call_id == "" then return nil end
  local cleaned = call_id:gsub("[^%w%-_]", "_")
  if cleaned == "" then return nil end
  return cleaned
end

-- ------------------------------------------------------------------
-- size check + serialisation
-- ------------------------------------------------------------------

-- Convert any output payload to a string for size checking + on-disk
-- write. Strings pass through; everything else is JSON-encoded so a
-- table-shaped output (e.g. a structured tool result) gets a
-- deterministic textual form.
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

-- Decide whether a payload deserves the dump-to-file path. Kept
-- separate from `dump()` so callers can short-circuit cheaply (the
-- common case is small output → no work).
---@param output any
---@return boolean
function M.should_dump(output)
  local s, _ = stringify(output)
  if not s then return false end
  return #s > M.INLINE_BUDGET
end

-- ------------------------------------------------------------------
-- summary string + on-disk write
-- ------------------------------------------------------------------

---@param payload string  -- already stringified
---@param path string
---@return string
local function summarise(payload, path)
  local total = #payload
  local preview_len = math.min(M.PREVIEW_BYTES, total)
  local preview = payload:sub(1, preview_len)
  return table.concat({
    "[Output written to " .. path .. "; " .. tostring(total)
      .. " bytes; truncated preview below]",
    "",
    preview,
    "",
    "... [output continues; full content at " .. path
      .. "; use `grep <pattern> " .. path .. "` or `head -n <N> "
      .. path .. "` to extract more] ...",
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

-- Write the optional `<call_id>.meta.json` companion. Best-effort —
-- failure to write the meta file does not cancel the main dump.
---@param meta_path string
---@param meta table
local function write_meta(meta_path, meta)
  local ok, encoded = pcall(json.encode, meta)
  if not ok then return end
  -- Ignore write errors — meta is debugging surface, not load-bearing.
  pcall(function() write_file(meta_path, encoded) end)
end

-- Dump the full output payload to disk + return the summary string and
-- the on-disk path. On any disk error returns `nil, err` so the caller
-- can degrade to the un-replaced path.
--
-- `args.tool` and `args.args` are optional metadata for the
-- `<call_id>.meta.json` companion (skipped when absent — the dump
-- itself doesn't need them).
---@param chat_id string|nil
---@param call_id string
---@param output any
---@param args table|nil  -- { tool?, args? }
---@return string|nil summary
---@return string|nil path
---@return string|nil err
function M.dump(chat_id, call_id, output, args)
  local payload, encode_err = stringify(output)
  if not payload then return nil, nil, encode_err end

  local root = data_root()
  if not root then
    return nil, nil, "no data root (NEFOR_DATA_HOME / XDG_DATA_HOME / HOME unset)"
  end

  local cid = safe_call_id(call_id)
  if not cid then return nil, nil, "missing call_id" end

  local scope_dir = root .. "/tool-results/" .. safe_scope(chat_id)
  ensure_dir(scope_dir)

  local path = scope_dir .. "/" .. cid .. ".txt"
  local ok, err = write_file(path, payload)
  if not ok then return nil, nil, err end

  if type(args) == "table" then
    local meta = {
      tool        = args.tool,
      args        = args.args,
      total_bytes = #payload,
      timestamp   = nefor.engine and nefor.engine.now and nefor.engine.now() or nil,
    }
    write_meta(scope_dir .. "/" .. cid .. ".meta.json", meta)
  end

  return summarise(payload, path), path, nil
end

-- Test-friendly: expose the private helpers.
M._stringify  = stringify
M._summarise  = summarise
M._safe_scope = safe_scope
M._data_root  = data_root

return M
