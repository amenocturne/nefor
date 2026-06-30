-- Generic instruction-file discovery primitives.
--
-- This library has no runtime side effects on its own. Callers choose when
-- to discover files, track reads, format reminders, and emit messages.

local M = {}

M.RESULT_LIMIT = 50
M.INSTRUCTION_FILENAMES = { "AGENTS.md", "CLAUDE.md" }

local function scope_key(chat_id)
  if type(chat_id) == "string" and chat_id ~= "" then return chat_id end
  return "_global"
end

local function bucket(tbl, chat_id)
  local key = scope_key(chat_id)
  local b = tbl[key]
  if not b then
    b = {}
    tbl[key] = b
  end
  return b
end

local function cwd()
  local pwd = os.getenv("PWD")
  if type(pwd) == "string" and pwd ~= "" then return pwd end
  return "."
end

local function to_absolute(p, base)
  if type(p) ~= "string" or p == "" then return "" end
  if p:sub(1, 1) == "/" then return p end
  local root = (type(base) == "string" and base ~= "") and base or cwd()
  return root .. "/" .. p
end

local function normalise(p)
  if p == "" then return "" end
  p = p:gsub("/+", "/")
  p = p:gsub("/%./", "/")
  if p:sub(1, 2) == "./" then p = p:sub(3) end
  if #p > 1 and p:sub(-2) == "/." then p = p:sub(1, -3) end
  if #p > 1 and p:sub(-1) == "/" then p = p:sub(1, -2) end
  return p
end

local function parent_dir(p)
  if p == "" or p == "/" then return "/" end
  local last_slash = p:match("()/[^/]*$")
  if last_slash == nil then return "/" end
  if last_slash == 1 then return "/" end
  return p:sub(1, last_slash - 1)
end

local function basename(p)
  return tostring(p or ""):match("([^/]+)$") or ""
end

local function shell_quote(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function is_directory(p)
  if type(p) ~= "string" or p == "" then return false end
  local ok = os.execute(string.format("[ -d %s ] 2>/dev/null", shell_quote(p)))
  return ok == true
end

local function is_instruction_file(path)
  local name = basename(path)
  for _, candidate in ipairs(M.INSTRUCTION_FILENAMES) do
    if name == candidate then return true end
  end
  return false
end

local function folder_for_path(path)
  local abs = normalise(to_absolute(path))
  if abs == "" then return "" end
  if is_directory(abs) then return abs end
  return parent_dir(abs)
end

local function git_root(path)
  local folder = folder_for_path(path)
  if folder == "" then return nil end
  local cmd = "git -C " .. shell_quote(folder) ..
              " rev-parse --show-toplevel 2>/dev/null"
  local pipe = io.popen(cmd, "r")
  if not pipe then return nil end
  local out = pipe:read("*l")
  pipe:close()
  if type(out) == "string" and out ~= "" then
    return normalise(out)
  end
  return nil
end

local function relative_path(root, path)
  if path == root then return "." end
  local prefix = root
  if prefix:sub(-1) ~= "/" then prefix = prefix .. "/" end
  if path:sub(1, #prefix) == prefix then return path:sub(#prefix + 1) end
  return path
end

M.NON_GIT_MAX_UP = 5
M.NON_GIT_MAX_DOWN = 4

local function file_exists(p)
  if type(p) ~= "string" or p == "" then return false end
  local ok = os.execute(string.format("[ -f %s ] 2>/dev/null", shell_quote(p)))
  return ok == true
end

local function has_instruction_file(dir)
  for _, name in ipairs(M.INSTRUCTION_FILENAMES) do
    if file_exists(dir .. "/" .. name) then return true end
  end
  return false
end

local function nearest_ancestor_with_instructions(folder, max_up)
  local cur = folder
  for _ = 1, max_up do
    if has_instruction_file(cur) then return cur end
    local up = parent_dir(cur)
    if up == cur then break end
    cur = up
  end
  return nil
end

local function resolve_scope(path, scope)
  local requested = scope == "subfolders" and "subfolders" or "auto"
  local folder = folder_for_path(path)
  if folder == "" then folder = normalise(to_absolute(path or ".")) end
  if requested == "auto" then
    local root = git_root(folder)
    if root then
      return root, "git_repo", nil
    end
    local ancestor = nearest_ancestor_with_instructions(folder, M.NON_GIT_MAX_UP)
    if ancestor then
      return ancestor, "nearest_ancestor", M.NON_GIT_MAX_DOWN
    end
  end
  return folder, "subfolders", M.NON_GIT_MAX_DOWN
end

local function find_instruction_files(root, max_depth)
  if type(root) ~= "string" or root == "" then return {}, 0 end
  local names = {}
  for _, name in ipairs(M.INSTRUCTION_FILENAMES) do
    names[#names + 1] = "-name " .. shell_quote(name)
  end
  local prunes = {
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".cache",
    "tmp",
  }
  local prune_terms = {}
  for _, name in ipairs(prunes) do
    prune_terms[#prune_terms + 1] = "-name " .. shell_quote(name)
  end
  local depth_flag = ""
  if type(max_depth) == "number" and max_depth > 0 then
    depth_flag = " -maxdepth " .. tostring(max_depth)
  end
  local cmd = "find " .. shell_quote(root) ..
              depth_flag ..
              " \\( " .. table.concat(prune_terms, " -o ") ..
              " \\) -prune -o -type f \\( " ..
              table.concat(names, " -o ") ..
              " \\) -print 2>/dev/null"
  local pipe = io.popen(cmd, "r")
  if not pipe then return {}, 0 end
  local files = {}
  for line in pipe:lines() do
    if type(line) == "string" and line ~= "" then
      files[#files + 1] = normalise(line)
    end
  end
  pipe:close()
  table.sort(files)
  local total = #files
  while #files > M.RESULT_LIMIT do
    table.remove(files)
  end
  return files, total
end

local function status_for(total, shown)
  if total == 0 then return "no instruction files found" end
  if shown >= total then return "all files shown" end
  return "truncated results, shown first " .. shown .. " out of " .. total
end

local function append_group(lines, title, files)
  if #files == 0 then return end
  lines[#lines + 1] = ""
  lines[#lines + 1] = title
  for _, file in ipairs(files) do
    lines[#lines + 1] = "- " .. file.relative_path
  end
end

local function resolve_arg_path(args, spec)
  local arg_name = spec.arg or "path"
  local raw = args[arg_name]
  if type(raw) ~= "string" or raw == "" then
    raw = spec.default
  end
  if type(raw) ~= "string" or raw == "" then return nil end
  local cwd_arg = spec.cwd_arg or "cwd"
  local base = type(args[cwd_arg]) == "string" and args[cwd_arg] or nil
  return normalise(to_absolute(raw, base))
end

function M.new()
  local reminded_scopes = {}
  local read_files = {}
  local tool_contexts = {}
  local S = {}

  function S.mark_read(chat_id, path)
    if type(path) ~= "string" or path == "" then return end
    local abs = normalise(to_absolute(path))
    if not is_instruction_file(abs) then return end
    bucket(read_files, chat_id)[abs] = true
  end

  local function status_for_file(chat_id, path)
    return bucket(read_files, chat_id)[path] and "read" or "unread"
  end

  function S.discover(path, opts)
    opts = opts or {}
    local root, resolved_scope, max_depth = resolve_scope(path or ".", opts.scope)
    local files, total = find_instruction_files(root, max_depth)
    local result_files = {}
    for _, path in ipairs(files) do
      local read_status = status_for_file(opts.chat_id, path)
      if not opts.unread_only or read_status ~= "read" then
        result_files[#result_files + 1] = {
          path = path,
          relative_path = relative_path(root, path),
          kind = basename(path),
          status = read_status,
        }
      end
    end
    return {
      scope = opts.scope == "subfolders" and "subfolders" or "auto",
      resolved_scope = resolved_scope,
      root = root,
      status = status_for(total, #files),
      files = result_files,
      total = total,
    }
  end

  function S.record_tool_contexts_from_advertise(body)
    if type(body) ~= "table" or type(body.tools) ~= "table" then return 0 end
    local count = 0
    for _, tool in ipairs(body.tools) do
      if type(tool) == "table"
          and type(tool.name) == "string"
          and type(tool.context) == "table" then
        tool_contexts[tool.name] = tool.context
        count = count + 1
      end
    end
    return count
  end

  function S.folders_for_tool_call(tool_name, args)
    if type(args) ~= "table" then args = {} end
    local context = tool_contexts[tool_name]
    if type(context) ~= "table" or type(context.folders) ~= "table" then
      return {}
    end

    local seen = {}
    local folders = {}
    for _, spec in ipairs(context.folders) do
      if type(spec) == "table" then
        local folder = nil
        if spec.from == "cwd" then
          local raw = args[spec.arg or "cwd"] or spec.default or "."
          folder = normalise(to_absolute(raw))
        else
          local resolved = resolve_arg_path(args, spec)
          if resolved then
            if spec.from == "file_path" then
              folder = parent_dir(resolved)
            elseif spec.from == "path_or_file" then
              folder = is_directory(resolved) and resolved or parent_dir(resolved)
            elseif spec.from == "directory" then
              folder = resolved
            end
          end
        end
        if type(folder) == "string" and folder ~= "" and not seen[folder] then
          seen[folder] = true
          folders[#folders + 1] = folder
        end
      end
    end
    return folders
  end

  function S.mark_read_for_tool_call(chat_id, tool_name, args)
    if tool_name ~= "read_file" or type(args) ~= "table" then return end
    local path = resolve_arg_path(args, { arg = "path", cwd_arg = "cwd" })
    if path then S.mark_read(chat_id, path) end
  end

  function S.emit_reminders_for_tool_call(tool_name, args, emitter)
    local chat_id = emitter.chat_id()
    S.mark_read_for_tool_call(chat_id, tool_name, args)

    local folders = S.folders_for_tool_call(tool_name, args)
    if #folders == 0 then return 0 end

    local reminders = bucket(reminded_scopes, chat_id)
    local count = 0
    for _, folder in ipairs(folders) do
      local result = S.discover(folder, { scope = "auto", chat_id = chat_id })
      if result.total and result.total > 0 then
        local key = result.resolved_scope .. ":" .. result.root
        if not reminders[key] then
          local text = S.format_reminder(result)
          if text then
            reminders[key] = true
            emitter.system(text, { path = result.root, dir = result.root })
            count = count + 1
          end
        end
      end
    end
    return count
  end


  function S._reset()
    reminded_scopes = {}
    read_files = {}
    tool_contexts = {}
  end

  function S._state(chat_id)
    return {
      reminded_scopes = bucket(reminded_scopes, chat_id),
      read_files = bucket(read_files, chat_id),
      tool_contexts = tool_contexts,
    }
  end

  S.format_discovery = M.format_discovery
  S.format_reminder = M.format_reminder
  S._normalise = normalise
  S._parent_dir = parent_dir
  S._to_absolute = to_absolute
  S._folder_for_path = folder_for_path
  S._git_root = git_root

  return S
end

function M.format_discovery(result)
  if type(result) ~= "table" then return "No instruction files found." end
  if type(result.files) ~= "table" or #result.files == 0 then
    return "No instruction files found for " .. tostring(result.root or ".") .. "."
  end

  local unread = {}
  local read = {}
  for _, file in ipairs(result.files) do
    if file.status == "read" then
      read[#read + 1] = file
    else
      unread[#unread + 1] = file
    end
  end

  local lines = {
    "Instruction files for " .. tostring(result.root),
    "status: " .. tostring(result.status),
  }
  append_group(lines, "unread", unread)
  append_group(lines, "read", read)
  if result.status and result.status:match("^truncated results") then
    lines[#lines + 1] = ""
    lines[#lines + 1] =
      "Hint: call again with scope=\"subfolders\" on a narrower path."
  end
  return table.concat(lines, "\n")
end

function M.format_reminder(result)
  if type(result) ~= "table" or type(result.files) ~= "table" or #result.files == 0 then
    return nil
  end
  return "Local instruction files available for " .. tostring(result.root) ..
         "\n" .. "status: " .. tostring(result.status) ..
         "\n\n" .. M.format_discovery(result):gsub("^Instruction files for [^\n]+\nstatus: [^\n]+\n\n?", "") ..
         "\n\nContents are not loaded automatically. Read any that seem relevant."
end

local default = M.new()

function M.mark_read(...) return default.mark_read(...) end
function M.discover(...) return default.discover(...) end
function M.record_tool_contexts_from_advertise(...)
  return default.record_tool_contexts_from_advertise(...)
end
function M.folders_for_tool_call(...) return default.folders_for_tool_call(...) end
function M.mark_read_for_tool_call(...) return default.mark_read_for_tool_call(...) end
function M.emit_reminders_for_tool_call(...)
  return default.emit_reminders_for_tool_call(...)
end
function M._reset() return default._reset() end
function M._state(...) return default._state(...) end

M._normalise = normalise
M._parent_dir = parent_dir
M._to_absolute = to_absolute
M._folder_for_path = folder_for_path
M._git_root = git_root

return M
