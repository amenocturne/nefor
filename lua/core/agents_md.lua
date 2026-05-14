-- starter/lib/agents_md.lua — smart per-tool-call AGENTS.md loader.
--
-- Per the lead-workflow spec §5: when a `read`/`write`/`edit` tool call
-- touches a file under `long/path/to/file`, the wrapper walks up from
-- the touched file's directory and, for each `AGENTS.md` not yet
-- loaded for this chat, appends its contents to the chat history with
-- a clear marker explaining why it appeared. The marker is
-- load-bearing: without it the model sees AGENTS.md content turning up
-- out of context and tries to interpret why, which steers it in the
-- wrong direction.
--
-- This module is the pure logic of the walk + dedup + emit. The
-- interception point — where to call `emit_for_tool_call` for outbound
-- `tool-gate.tool.invoke` envelopes — lives in
-- `starter/tool-gate/init.lua`, the canonical wrapper between the
-- caller (lead / agent reasoner) and the tool plugin's binary.
--
-- ## What constitutes a path-touching tool call
--
-- A tool whose args object carries one of:
--   * `path`        (read_file, write_file, edit, …)
--   * `file_path`   (alternate naming used by some edit-shaped tools)
--   * `target_path` (occasional alias)
-- and whose value resolves to a file (or whose parent dir exists).
-- `bash`, the spawn-graph tool, and other non-file-targeting tools have
-- no such field and are no-op'd.
--
-- ## Walk
--
-- Given the tool's `path` arg:
--   1. Resolve to absolute (cwd-relative if the path isn't already
--      `/`-rooted).
--   2. Take the parent dir as the starting point. If the path itself is
--      a directory, that's the starting point.
--   3. Walk up to the filesystem root or a 10-level safety cap,
--      collecting each ancestor dir.
--   4. Reverse so the OUTERMOST dir comes first — outer rules govern
--      and the inner dirs refine, matching the order a human would read
--      the project's tree from root → leaf.
--   5. For each ancestor dir, check if `<dir>/AGENTS.md` exists AND has
--      not yet been loaded for this chat_id. If so, queue.
--
-- ## Dedup state
--
-- Loaded set is keyed by chat_id (or a `_global` fallback when chat_id
-- isn't surfaced — same pattern as `tool_output_dump.lua`'s
-- `_unscoped/`). The set survives the lifetime of this Lua VM, which
-- is the lifetime of the chat from the wrapper's point of view: a
-- `/new` mints a fresh chat_id (different bucket); a `/resume` replays
-- the prior chat history including the AGENTS.md emissions, which
-- re-emit identically — same bytes, same marker, model sees what it
-- saw before.
--
-- ## Marker shape
--
-- Per spec §5: explicit "automatic context" framing so the model
-- doesn't misinterpret the appearance of AGENTS.md content as a user
-- request.
--
--   [Loaded /abs/path/AGENTS.md because tool call touched a file in
--    /abs/path/. This is project guidance for that directory, not a
--    user request.]
--
--   <contents of AGENTS.md>
--
-- ## Failure modes
--
-- File-read errors degrade silently — a permission-denied or
-- transiently-unreadable AGENTS.md is logged at warn level and skipped
-- (the model proceeds with the tool call as if AGENTS.md weren't there;
-- a noisy error in chat would be worse than missing context). Empty
-- AGENTS.md files are skipped — emitting just the marker without a
-- body is pure noise.

local json = nefor.json

local M = {}

-- Safety cap on the ancestor walk. AGENTS.md genuinely lives at
-- project-root depth (3-6 dirs deep on a typical monorepo); 10 levels
-- catches every realistic project layout while bounding the
-- (potential) cost of a `realpath`-shaped traversal across symlink
-- chains.
M.MAX_WALK_DEPTH = 10

-- ------------------------------------------------------------------
-- in-memory loaded set, keyed by chat_id (or `_global`).
-- ------------------------------------------------------------------

-- loaded_set[scope] = { [absolute_agents_md_path] = true, ... }
local loaded_set = {}

local function scope_key(chat_id)
  if type(chat_id) == "string" and chat_id ~= "" then return chat_id end
  return "_global"
end

local function bucket(chat_id)
  local key = scope_key(chat_id)
  local b = loaded_set[key]
  if not b then
    b = {}
    loaded_set[key] = b
  end
  return b
end

-- ------------------------------------------------------------------
-- path utilities
-- ------------------------------------------------------------------

-- Best-effort cwd. The starter doesn't pin a cwd globally, so we use
-- $PWD when set (process env at launch time, what the user actually
-- expected as "current dir") and fall back to "." which `io.open`
-- happily resolves against the runtime cwd. This is the same
-- convention chat.lua's `at_path_resolve` uses.
local function cwd()
  local pwd = os.getenv("PWD")
  if type(pwd) == "string" and pwd ~= "" then return pwd end
  return "."
end

-- Make `p` absolute by prepending cwd if it isn't already `/`-rooted.
-- Does NOT resolve symlinks or `..` segments — that would require
-- `realpath` which isn't portable from pure Lua. The wrapper above
-- normalises trailing slashes and `.` segments only.
---@param p string
---@return string
local function to_absolute(p)
  if type(p) ~= "string" or p == "" then return "" end
  if p:sub(1, 1) ~= "/" then p = cwd() .. "/" .. p end
  return p
end

-- Strip `./` segments and trailing slashes. Leaves `..` segments alone
-- — resolving them safely needs filesystem access (because of
-- symlinks). For AGENTS.md walking, leaving `..` in the path means a
-- path like `/a/b/../c/x.txt` walks `/a/b/..`, `/a/b`, `/a`, `/` —
-- which over-loads /a/b's AGENTS.md but never loads anything wrong.
-- That's fine for v0.1.
---@param p string
---@return string
local function normalise(p)
  if p == "" then return "" end
  -- Collapse runs of slashes.
  p = p:gsub("/+", "/")
  -- Strip `./` segments.
  p = p:gsub("/%./", "/")
  if p:sub(1, 2) == "./" then p = p:sub(3) end
  -- Strip trailing slash unless the whole path is just "/".
  if #p > 1 and p:sub(-1) == "/" then p = p:sub(1, -2) end
  return p
end

-- Return the parent directory of an absolute path. `/a/b/c.txt` → `/a/b`.
-- `/a` → `/`. `/` → `/`. The path is assumed normalised.
---@param p string
---@return string
local function parent_dir(p)
  if p == "" or p == "/" then return "/" end
  local last_slash = p:match("()/[^/]*$")
  if last_slash == nil then return "/" end
  if last_slash == 1 then return "/" end
  return p:sub(1, last_slash - 1)
end

-- Decide whether a path resolves to a directory. Lua doesn't expose
-- stat() without LuaFileSystem, so we shell out to `[ -d <path> ]` —
-- portable enough across the macOS / linux targets the engine runs
-- on. The earlier "try io.open in read mode" trick is unreliable
-- across libcs: on macOS `fopen(dir, "r")` succeeds (you can read
-- raw bytes off a directory FD on HFS+/APFS), so a successful open
-- doesn't tell us "this is a file".
---@param p string
---@return boolean
local function is_directory(p)
  if type(p) ~= "string" or p == "" then return false end
  -- `os.execute` returns true on exit-0 in Lua 5.2+, false otherwise.
  -- The %q quoting protects against spaces / shell metas in the path.
  local ok = os.execute(string.format("[ -d %q ] 2>/dev/null", p))
  return ok == true
end

-- Build the list of ancestor dirs to check for an AGENTS.md, given a
-- file path. Returns the dirs in OUTERMOST-FIRST order (root → leaf)
-- so callers iterating the list emit the outermost AGENTS.md first.
--
-- Stops at `/`. The MAX_WALK_DEPTH cap protects against pathological
-- inputs (paths with hundreds of `..` segments, malformed paths) by
-- bounding the loop.
---@param file_path string
---@return string[]
function M.paths_to_check(file_path)
  if type(file_path) ~= "string" or file_path == "" then return {} end

  local abs = normalise(to_absolute(file_path))
  if abs == "" then return {} end

  local start_dir
  if is_directory(abs) then
    start_dir = abs
  else
    start_dir = parent_dir(abs)
  end

  local dirs = {}
  local cur = start_dir
  local depth = 0
  while depth < M.MAX_WALK_DEPTH do
    dirs[#dirs + 1] = cur
    if cur == "/" then break end
    cur = parent_dir(cur)
    depth = depth + 1
  end

  -- Reverse so outermost (root-most) comes first.
  local reversed = {}
  for i = #dirs, 1, -1 do
    reversed[#reversed + 1] = dirs[i]
  end
  return reversed
end

-- ------------------------------------------------------------------
-- AGENTS.md read + dedup
-- ------------------------------------------------------------------

-- Read the AGENTS.md file at `dir`. Returns:
--   contents (string) on success
--   nil, nil           when the file doesn't exist (the common case)
--   nil, err           on a real read failure (logged warn at caller)
local function read_agents_md(dir)
  local path = (dir == "/" and "/AGENTS.md") or (dir .. "/AGENTS.md")
  local f, err = io.open(path, "r")
  if not f then
    -- ENOENT is the dominant case — every dir on the walk that
    -- doesn't have an AGENTS.md hits this. Distinguishing ENOENT from
    -- a real error from pure Lua isn't reliable, so we treat any open
    -- failure as "no AGENTS.md here" and let the caller move on.
    return nil, nil, path
  end
  local data = f:read("*a")
  f:close()
  if data == nil then return nil, "read returned nil", path end
  return data, nil, path
end

-- Walk the dirs list, return `{ { path, contents }, ... }` for any
-- AGENTS.md not yet loaded for `chat_id`. Outermost-first preserved
-- from the input list.
---@param chat_id string|nil
---@param dirs string[]
---@return { path: string, contents: string }[]
function M.find_unloaded_agents_md(chat_id, dirs)
  local b = bucket(chat_id)
  local found = {}
  for _, dir in ipairs(dirs) do
    local agents_path = (dir == "/" and "/AGENTS.md") or (dir .. "/AGENTS.md")
    if not b[agents_path] then
      local contents, err = read_agents_md(dir)
      if contents and #contents > 0 then
        found[#found + 1] = { path = agents_path, contents = contents, dir = dir }
      elseif err then
        nefor.log.warn("agents_md: read failed; skipping", {
          path = agents_path, error = err,
        })
      end
    end
  end
  return found
end

-- Mark each path in `paths` as loaded for `chat_id`. Idempotent.
---@param chat_id string|nil
---@param paths string[]
function M.mark_loaded(chat_id, paths)
  local b = bucket(chat_id)
  for _, p in ipairs(paths) do
    if type(p) == "string" and p ~= "" then b[p] = true end
  end
end

-- ------------------------------------------------------------------
-- envelope construction
-- ------------------------------------------------------------------

-- Build the marker + body string the model sees in chat.
---@param agents_path string  -- absolute path to AGENTS.md
---@param dir string          -- absolute path to the dir holding AGENTS.md
---@param contents string
---@return string
local function format_message(agents_path, dir, contents)
  -- The bracketed line is the "automatic context" marker the spec
  -- calls load-bearing. The phrasing is direct about WHAT and WHY so
  -- the model doesn't try to interpret the appearance of AGENTS.md as
  -- a user instruction.
  return "[Loaded " .. agents_path
    .. " because tool call touched a file in " .. dir
    .. ". This is project guidance for that directory, not a user request.]"
    .. "\n\n" .. contents
end

-- Decide whether a tool call is "path-touching". Checks the args
-- object for a `path`, `file_path`, or `target_path` field.
-- Returns the path string if found, else nil.
---@param tool_name string
---@param args table|nil
---@return string|nil
function M.extract_path(tool_name, args)
  if type(args) ~= "table" then return nil end
  -- Heuristic by field name; tool_name is currently advisory only —
  -- any tool that carries a path-shaped field is treated as
  -- path-touching. Listed in priority order: `path` is the canonical
  -- (read_file, write_file), `file_path` and `target_path` are
  -- aliases used by some edit-shaped tools.
  for _, key in ipairs({ "path", "file_path", "target_path" }) do
    local v = args[key]
    if type(v) == "string" and v ~= "" then return v end
  end
  return nil
end

-- Orchestrator: given a tool call, walk the path's ancestor dirs,
-- find unloaded AGENTS.md, emit chat.message.append envelopes via
-- `emitter`, and mark them loaded for the chat. `emitter(body)` is
-- called once per AGENTS.md, in OUTERMOST-FIRST order. The caller
-- supplies the emit function so this module stays decoupled from
-- envelope.lua (and so tests can capture emissions without driving the
-- whole bus).
--
-- No-op when:
--   * the tool call has no path-shaped arg
--   * no AGENTS.md exists anywhere in the ancestor chain
--   * every ancestor's AGENTS.md is already loaded
--
-- Returns the count of AGENTS.md actually emitted (0 in any of the
-- no-op cases above), useful for tests.
---@param chat_id string|nil
---@param tool_name string
---@param args table|nil
---@param emitter fun(body: table)
---@return integer
function M.emit_for_tool_call(chat_id, tool_name, args, emitter)
  local file_path = M.extract_path(tool_name, args)
  if not file_path then return 0 end

  local dirs = M.paths_to_check(file_path)
  if #dirs == 0 then return 0 end

  local found = M.find_unloaded_agents_md(chat_id, dirs)
  if #found == 0 then return 0 end

  -- Emit outermost-first (already that order from find_unloaded).
  local marked = {}
  for _, e in ipairs(found) do
    local text = format_message(e.path, e.dir, e.contents)
    emitter({
      kind = "chat.message.append",
      role = "system",
      text = text,
    })
    marked[#marked + 1] = e.path
  end
  M.mark_loaded(chat_id, marked)

  return #found
end

-- Test-only: reset the in-memory loaded set.
function M._reset()
  loaded_set = {}
end

-- Test-only: peek at the loaded set for a given chat_id.
function M._loaded_set(chat_id)
  return bucket(chat_id)
end

-- Expose internals for unit tests.
M._normalise   = normalise
M._parent_dir  = parent_dir
M._to_absolute = to_absolute
M._format_message = format_message

return M
