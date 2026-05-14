-- Smart per-tool-call AGENTS.md loader. When a path-touching tool
-- call (read_file, write_file, edit, …) targets a file, walk up the
-- file's ancestor dirs and emit each AGENTS.md not yet loaded for
-- this chat as a `chat.message.append { role = "system" }` envelope.
-- Outermost-first so the model reads root → leaf, matching the order
-- a human reads the project tree.

local M = {}

-- AGENTS.md genuinely lives at project-root depth (3-6 dirs on a
-- typical monorepo); 10 catches every realistic layout while bounding
-- the cost of a pathological `..`-heavy or symlink-chain traversal.
M.MAX_WALK_DEPTH = 10

-- loaded_set[scope] = { [absolute_agents_md_path] = true, ... }
-- Survives the lifetime of the Lua VM = lifetime of the chat as far
-- as the wrapper sees it; `/new` mints a fresh chat_id (different
-- bucket) and `/resume` replays the chat history so the same
-- emissions re-emit identically.
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

-- Best-effort cwd. Starter doesn't pin a cwd globally; $PWD is the
-- process env at launch time — same convention chat.lua uses for
-- `at_path_resolve`. Falls back to "." which `io.open` resolves
-- against runtime cwd.
local function cwd()
  local pwd = os.getenv("PWD")
  if type(pwd) == "string" and pwd ~= "" then return pwd end
  return "."
end

---@param p string
---@return string
local function to_absolute(p)
  if type(p) ~= "string" or p == "" then return "" end
  if p:sub(1, 1) ~= "/" then p = cwd() .. "/" .. p end
  return p
end

-- Strip `./` segments and trailing slashes. Leaves `..` alone —
-- resolving safely needs filesystem access (symlinks). For walking
-- purposes, leaving `..` in means `/a/b/../c/x.txt` walks `/a/b/..`,
-- `/a/b`, `/a`, `/` — over-loads /a/b's AGENTS.md but never loads
-- anything wrong.
---@param p string
---@return string
local function normalise(p)
  if p == "" then return "" end
  p = p:gsub("/+", "/")
  p = p:gsub("/%./", "/")
  if p:sub(1, 2) == "./" then p = p:sub(3) end
  if #p > 1 and p:sub(-1) == "/" then p = p:sub(1, -2) end
  return p
end

-- Parent directory of an absolute path. `/a/b/c.txt` → `/a/b`,
-- `/a` → `/`, `/` → `/`. Path assumed normalised.
---@param p string
---@return string
local function parent_dir(p)
  if p == "" or p == "/" then return "/" end
  local last_slash = p:match("()/[^/]*$")
  if last_slash == nil then return "/" end
  if last_slash == 1 then return "/" end
  return p:sub(1, last_slash - 1)
end

-- Shell out to `[ -d <path> ]`: Lua has no stat() without LuaFileSystem,
-- and the io.open trick is unreliable across libcs (macOS's fopen
-- happily opens a directory in read mode on APFS).
---@param p string
---@return boolean
local function is_directory(p)
  if type(p) ~= "string" or p == "" then return false end
  -- %q quoting protects against spaces / shell metas in the path.
  local ok = os.execute(string.format("[ -d %q ] 2>/dev/null", p))
  return ok == true
end

-- Ancestor dirs to check for an AGENTS.md, given a file or directory
-- path. OUTERMOST-FIRST so callers emit root-most first (outer rules
-- govern, inner refines).
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

  local reversed = {}
  for i = #dirs, 1, -1 do
    reversed[#reversed + 1] = dirs[i]
  end
  return reversed
end

-- ENOENT is the dominant case — every dir on the walk without an
-- AGENTS.md hits this. Distinguishing it from a real error from pure
-- Lua isn't reliable, so any open failure is treated as "no AGENTS.md
-- here" and the caller moves on.
local function read_agents_md(dir)
  local path = (dir == "/" and "/AGENTS.md") or (dir .. "/AGENTS.md")
  local f, err = io.open(path, "r")
  if not f then return nil, nil end
  local data = f:read("*a")
  f:close()
  if data == nil then return nil, "read returned nil" end
  return data, nil
end

---@param chat_id string|nil
---@param dirs string[]
---@return { path: string, contents: string, dir: string }[]
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

---@param chat_id string|nil
---@param paths string[]
function M.mark_loaded(chat_id, paths)
  local b = bucket(chat_id)
  for _, p in ipairs(paths) do
    if type(p) == "string" and p ~= "" then b[p] = true end
  end
end

-- Marker is load-bearing: without it the model sees AGENTS.md content
-- turning up out of context and tries to interpret why, which steers
-- it wrong. The bracketed line names WHAT and WHY in a shape the
-- model treats as automatic context, not a user request.
---@param agents_path string
---@param dir string
---@param contents string
---@return string
local function format_message(agents_path, dir, contents)
  return "[Loaded " .. agents_path
    .. " because tool call touched a file in " .. dir
    .. ". This is project guidance for that directory, not a user request.]"
    .. "\n\n" .. contents
end

-- Decide whether a tool call is "path-touching" by inspecting args for
-- a path-shaped field. tool_name is advisory only — any tool that
-- carries a path-shaped field counts. `path` is canonical (read/write),
-- `file_path` and `target_path` are aliases used by edit-shaped tools.
---@param tool_name string
---@param args table|nil
---@return string|nil
function M.extract_path(tool_name, args)
  if type(args) ~= "table" then return nil end
  for _, key in ipairs({ "path", "file_path", "target_path" }) do
    local v = args[key]
    if type(v) == "string" and v ~= "" then return v end
  end
  return nil
end

-- Walk, dedup, emit. `emitter(body)` is called once per AGENTS.md in
-- OUTERMOST-FIRST order; caller supplies it so this module stays
-- decoupled from envelope.lua (and tests can capture emissions
-- without driving the bus). Returns the count actually emitted.
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

  local marked = {}
  for _, e in ipairs(found) do
    local text = format_message(e.path, e.dir, e.contents)
    emitter({
      kind    = "chat.message.append",
      role    = "system",
      text    = text,
      -- Stamp the chat_id so the chat surface can route sub-agent
      -- AGENTS.md emissions to the DAG sidebar instead of the main
      -- transcript. The chat reducer maps chat_id → (run_id, node_id)
      -- via `graph.node.chat.bound`; emissions whose chat_id is a
      -- known sub-chat surface as a node-row "last tool" line, not
      -- as a free-standing entry in the main chat.
      chat_id = chat_id,
      path    = e.path,
      dir     = e.dir,
    })
    marked[#marked + 1] = e.path
  end
  M.mark_loaded(chat_id, marked)

  return #found
end

-- Test-only: reset module state + peek at the loaded set.
function M._reset()
  loaded_set = {}
end

function M._loaded_set(chat_id)
  return bucket(chat_id)
end

M._normalise     = normalise
M._parent_dir    = parent_dir
M._to_absolute   = to_absolute
M._format_message = format_message

return M
