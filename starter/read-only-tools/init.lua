-- starter/read-only-tools/init.lua — read-only investigation tools.
--
-- Two tools advertised through tool-gate as source `read-only-tools`:
--
--   * `list_dir`   — args { path }. Returns a one-line-per-entry listing
--                    of `path`, with `(d)` / `(f)` prefixes for dirs vs
--                    files. Uses the engine's nefor.fs.list_dir binding,
--                    so it can't traverse outside whatever the engine
--                    process can already see.
--
--   * `search_text` — args { pattern, path?, max_results? }. Shells out
--                    via nefor.process.run to `rg -n --color=never`
--                    (preferred) or `grep -rn --color=never` as fallback.
--                    Path defaults to ".". Pure read; the subprocess
--                    has no write semantics.
--
--   * `python-read` — advertised as the single Python analysis surface for
--                    complex read-only workspace inspection. MVP semantics:
--                    read workspace, write scratch only, deny network,
--                    subprocess, dynamic code, and arbitrary imports.
--
--   * `mirror-projects` — typed read-only wrapper around the Mirror project
--                         task CLI. This gives agents a named task-context
--                         surface instead of making them remember bash
--                         incantations or fall back to manual task searches.
--
-- Layered so an explorer / reviewer agent can investigate the codebase
-- without needing the full `bash` surface (which is a sandbox-escape
-- hatch via shell composition).

local json = nefor.json

local envelope = require("core.envelope")
local emit_as  = envelope.emit_as
local instruction_files = require("libs.instruction-files")

local INSTRUCTIONS_DIR = (rawget(_G, "NEFOR_CONFIG_DIR") or ".") .. "/instructions"

local SOURCE_NAME = "read-only-tools"

local function emit_ok(firing_id, text)
  emit_as(SOURCE_NAME, nil, {
    kind   = "tool.result",
    id     = firing_id,
    output = tostring(text or ""),
  })
end

local function emit_err(firing_id, err)
  emit_as(SOURCE_NAME, nil, {
    kind  = "tool.result",
    id    = firing_id,
    error = tostring(err),
  })
end

local function tool_list_dir(firing_id, args)
  local path = args and args.path
  if type(path) ~= "string" or #path == 0 then
    emit_err(firing_id, "list_dir: args.path must be a non-empty string")
    return
  end
  local entries, err = nefor.fs.list_dir(path)
  if entries == nil then
    emit_err(firing_id, "list_dir: " .. tostring(err or "unknown error"))
    return
  end
  table.sort(entries, function(a, b)
    if a.is_dir ~= b.is_dir then return a.is_dir end
    return a.name < b.name
  end)
  local lines = {}
  for _, e in ipairs(entries) do
    lines[#lines + 1] = (e.is_dir and "(d) " or "(f) ") .. e.name
  end
  if #lines == 0 then lines[1] = "(empty directory)" end
  emit_ok(firing_id, table.concat(lines, "\n"))
end

-- Pick the search backend on first use. rg is preferred — faster, sane
-- defaults, respects .gitignore. Falls back to POSIX grep -rn.
local search_cmd = nil
local function resolve_search_cmd()
  if search_cmd ~= nil then return search_cmd end
  local probe = nefor.process.run { cmd = "rg", args = { "--version" } }
  if type(probe) == "table" and probe.code == 0 then
    search_cmd = "rg"
  else
    search_cmd = "grep"
  end
  return search_cmd
end

local function tool_search_text(firing_id, args)
  local pattern = args and args.pattern
  if type(pattern) ~= "string" or #pattern == 0 then
    emit_err(firing_id, "search_text: args.pattern must be a non-empty string")
    return
  end
  local path = (type(args.path) == "string" and #args.path > 0) and args.path or "."
  local cap  = tonumber(args.max_results) or 200
  if cap < 1 then cap = 1 end
  if cap > 2000 then cap = 2000 end

  local backend = resolve_search_cmd()
  local argv
  if backend == "rg" then
    argv = { "-n", "--color=never", "--max-count", tostring(cap),
             "--max-columns", "500", "--max-columns-preview",
             "--", pattern, path }
  else
    argv = { "-rn", "--color=never", "--", pattern, path }
  end
  local out = nefor.process.run { cmd = backend, args = argv }
  if type(out) ~= "table" then
    emit_err(firing_id, "search_text: nefor.process.run returned non-table")
    return
  end
  -- rg / grep both exit 1 when no matches are found — that's not an
  -- error from the agent's perspective. Distinguish by stderr length.
  if out.code ~= 0 and out.code ~= 1 then
    emit_err(firing_id, string.format(
      "search_text: %s exited %d: %s",
      backend, out.code, tostring(out.stderr or "")))
    return
  end
  local stdout = tostring(out.stdout or "")
  if #stdout == 0 then
    emit_ok(firing_id, "(no matches)")
    return
  end
  -- Truncate to cap lines defensively (rg's --max-count is per-file,
  -- not total). One line per match is the expected shape.
  local MAX_OUTPUT_BYTES = 256 * 1024
  local truncated = {}
  local n = 0
  local total_bytes = 0
  for line in stdout:gmatch("[^\n]+") do
    n = n + 1
    if n > cap then
      truncated[#truncated + 1] = "[...truncated at line cap]"
      break
    end
    total_bytes = total_bytes + #line + 1
    if total_bytes > MAX_OUTPUT_BYTES then
      truncated[#truncated + 1] = "[...truncated at 256KB]"
      break
    end
    truncated[#truncated + 1] = line
  end
  emit_ok(firing_id, table.concat(truncated, "\n"))
end

local function tool_python_read(firing_id, _args)
  emit_err(firing_id,
    "python-read: sandboxed Python analysis is not available in this MVP. " ..
    "Use Bash/read tools for simple inspection; do not route raw Python, " ..
    "uv, pip, or pytest through Bash for analysis.")
end

local function add_flag(argv, flag, value)
  if type(value) == "string" and #value > 0 then
    argv[#argv + 1] = flag
    argv[#argv + 1] = value
  end
end

local function add_bool(argv, flag, value)
  if value == true then
    argv[#argv + 1] = flag
  end
end

local function add_limit(argv, value)
  local n = tonumber(value)
  if n == nil then return end
  if n < 1 then n = 1 end
  if n > 100 then n = 100 end
  argv[#argv + 1] = "--limit"
  argv[#argv + 1] = tostring(math.floor(n))
end

local function tool_mirror_projects(firing_id, args)
  local action = args and args.action
  if type(action) ~= "string" or #action == 0 then
    emit_err(firing_id, "mirror-projects: args.action must be one of list, tasks, show, find")
    return
  end

  local argv = {}
  if action == "list" then
    argv = { "list", "--json" }
    add_flag(argv, "--query", args.query)
    add_limit(argv, args.limit)
  elseif action == "tasks" then
    argv = { "tasks", "--json" }
    add_flag(argv, "--project", args.project)
    add_flag(argv, "--status", args.status)
    add_bool(argv, "--blocked", args.blocked)
  elseif action == "show" then
    local task_id = args.task_id
    if type(task_id) ~= "string" or #task_id == 0 then
      emit_err(firing_id, "mirror-projects: action=show requires args.task_id")
      return
    end
    argv = { "show", task_id, "--json" }
  elseif action == "find" then
    local query = args.query
    if type(query) ~= "string" or #query == 0 then
      emit_err(firing_id, "mirror-projects: action=find requires args.query")
      return
    end
    argv = { "find", query, "--json" }
  else
    emit_err(firing_id, "mirror-projects: unsupported read-only action '" ..
      tostring(action) .. "'")
    return
  end

  local out = nefor.process.run { cmd = "mirror-projects", args = argv }
  if type(out) ~= "table" then
    emit_err(firing_id, "mirror-projects: nefor.process.run returned non-table")
    return
  end
  if out.code ~= 0 then
    emit_err(firing_id, string.format(
      "mirror-projects %s exited %d: %s",
      action, out.code, tostring(out.stderr or "")))
    return
  end
  emit_ok(firing_id, tostring(out.stdout or ""))
end

local function read_one_instruction(raw_name)
  local name = raw_name:gsub("%.md$", "")
  local path = INSTRUCTIONS_DIR .. "/" .. name .. ".md"
  local f, err = io.open(path, "r")
  if not f then
    return nil, tostring(err or "file not found: " .. path)
  end
  local content = f:read("*a")
  f:close()
  if not content or #content == 0 then
    return nil, "empty file at " .. path
  end
  return content, nil
end

local function tool_instructions(firing_id, args)
  local name = args and args.name
  if type(name) == "string" and #name > 0 then
    local content, err = read_one_instruction(name)
    if not content then
      emit_err(firing_id, "instructions: " .. err)
      return
    end
    emit_ok(firing_id, content)
    return
  end
  if type(name) == "table" and #name > 0 then
    local parts = {}
    for _, n in ipairs(name) do
      if type(n) == "string" and #n > 0 then
        local content, err = read_one_instruction(n)
        if content then
          parts[#parts + 1] = "--- instruction: " .. n:gsub("%.md$", "") .. " ---\n" .. content
        else
          parts[#parts + 1] = "--- instruction: " .. n:gsub("%.md$", "") .. " ---\n[error: " .. err .. "]"
        end
      end
    end
    if #parts == 0 then
      emit_err(firing_id, "instructions: name array contained no valid entries")
      return
    end
    emit_ok(firing_id, table.concat(parts, "\n\n"))
    return
  end
  emit_err(firing_id, "instructions: args.name must be a non-empty string or array of strings")
end

local function tool_discover_instruction_files(firing_id, args)
  args = args or {}
  local path = type(args.path) == "string" and args.path or "."
  local scope = args.scope == "subfolders" and "subfolders" or "auto"
  local unread_only = args.unread_only == true
  local result = instruction_files.discover(path, {
    scope = scope,
    unread_only = unread_only,
  })
  emit_ok(firing_id, instruction_files.format_discovery(result))
end

local TOOL_HANDLERS = {
  list_dir                   = tool_list_dir,
  search_text                = tool_search_text,
  ["python-read"]            = tool_python_read,
  ["mirror-projects"] = tool_mirror_projects,
  instructions               = tool_instructions,
  discover_instruction_files = tool_discover_instruction_files,
}

local function handle_tool_invoke(body)
  local firing_id = body.id
  if type(firing_id) ~= "string" then return end
  local handler = TOOL_HANDLERS[body.name]
  if not handler then
    emit_err(firing_id, "read-only-tools: unknown tool '" ..
      tostring(body.name) .. "'")
    return
  end
  -- We advertised the tool; the caller is owed a tool.result. A handler
  -- crash without this wrapper produces no envelope on the bus, which
  -- the agent reasoner reads as "still running" and hangs forever.
  local ok, err = pcall(handler, firing_id, body.args or {})
  if not ok then
    emit_err(firing_id, "read-only-tools." .. tostring(body.name) ..
      ": handler raised: " .. tostring(err))
  end
end

local function tool_schemas()
  return {
    {
      name = "list_dir",
      description =
        "List the immediate children of a directory. Returns one entry " ..
        "per line, prefixed with `(d)` for directories and `(f)` for " ..
        "files. Read-only.",
      parameters = {
        type = "object",
        properties = {
          path = { type = "string",
                   description = "Directory path. Use '.' for the workspace root." },
        },
        required = { "path" },
      },
      context = {
        folders = {
          { from = "directory", arg = "path" },
        },
      },
    },
    {
      name = "search_text",
      description =
        "Search for a regex pattern in files under a path (recursively). " ..
        "Returns matching lines as `path:line:match`. Uses ripgrep when " ..
        "available, POSIX grep -rn otherwise. Read-only.",
      parameters = {
        type = "object",
        properties = {
          pattern = { type = "string",
                      description = "Regex pattern (ERE / rg syntax)." },
          path = { type = "string",
                   description = "Search root (file or directory). Defaults to '.'." },
          max_results = { type = "integer",
                          description = "Cap on returned lines (default 200, max 2000)." },
        },
        required = { "pattern" },
      },
      context = {
        folders = {
          { from = "path_or_file", arg = "path", default = "." },
        },
      },
    },
    {
      name = "python-read",
      description =
        "Run complex read-only Python analysis over workspace files. " ..
        "Prefer Bash/read tools for simple inspection. Do not use raw " ..
        "Python, uv, pip, or pytest through Bash for analysis. MVP " ..
        "restrictions: read workspace, write scratch only, no network, " ..
        "subprocesses, dynamic code, or arbitrary imports.",
      parameters = {
        type = "object",
        properties = {
          task = { type = "string",
                   description = "Read-only analysis request to perform." },
        },
        required = { "task" },
      },
    },
    {
      name = "mirror-projects",
      description =
        "Read Mirror project/task context through the mirror-projects CLI. " ..
        "Use this before manual task-folder searches for project-shaped " ..
        "work, task IDs, project boards, task status, dependencies, " ..
        "blocked work, or next-task questions. Read-only actions: list, " ..
        "tasks, show, find.",
      parameters = {
        type = "object",
        properties = {
          action = { type = "string",
                     description = "Read-only action: list, tasks, show, or find." },
          project = { type = "string",
                      description = "Project slug for action=tasks." },
          task_id = { type = "string",
                      description = "Task id for action=show." },
          query = { type = "string",
                    description = "Project/task search text for action=list or action=find." },
          status = { type = "string",
                     description = "Optional task status filter for action=tasks." },
          blocked = { type = "boolean",
                      description = "When true with action=tasks, only blocked tasks." },
          limit = { type = "integer",
                    description = "Optional result cap for action=list, max 100." },
        },
        required = { "action" },
      },
    },
    {
      name = "instructions",
      description =
        "Read one or more instruction files by name. When the " ..
        "system prompt says to read instructions (e.g. 'instruction:dev-mode.md'), " ..
        "call this tool with that name. Pass an array to load multiple " ..
        "instructions in one call. The .md extension is optional.",
      parameters = {
        type = "object",
        properties = {
          name = {
            oneOf = {
              { type = "string" },
              { type = "array", items = { type = "string" } },
            },
            description = "Instruction name or array of names, e.g. 'dev-mode' or ['dev-mode', 'dev-philosophy', 'workspace-routing'].",
          },
        },
        required = { "name" },
      },
    },
    {
      name = "discover_instruction_files",
      description =
        "List AGENTS.md and CLAUDE.md instruction files available near " ..
        "a path. Does not read file contents. Use ordinary read_file on " ..
        "any file that seems relevant.",
      parameters = {
        type = "object",
        properties = {
          path = {
            type = "string",
            description = "Directory or file path to inspect. Defaults to '.'.",
          },
          scope = {
            type = "string",
            enum = { "auto", "subfolders" },
            description =
              "auto: git repo when inside one, otherwise subfolders. " ..
              "subfolders: only below path.",
          },
          unread_only = {
            type = "boolean",
            description = "Only show instruction files not read this session.",
          },
        },
      },
    },
  }
end

local advertised = false
local function advertise_tools(gate_name)
  if advertised then return end
  advertised = true
  emit_as(SOURCE_NAME, nil, {
    kind   = (gate_name or "tool-gate") .. ".tools.advertise",
    source = SOURCE_NAME,
    tools  = tool_schemas(),
  })
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end
  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

  if kind == SOURCE_NAME .. ".tool.invoke" then
    handle_tool_invoke(body)
    return
  end
  if kind == "tool-gate.hello" then
    advertise_tools("tool-gate")
    return
  end
end

return {
  name        = SOURCE_NAME,
  receive_msg = receive_msg,
  send_msg    = function(_) end,
}
