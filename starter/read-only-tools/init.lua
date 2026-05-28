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
--   * `search_text` — args { pattern, path?, max_results?,
--                    case_insensitive?, files_only? }. Shells out via
--                    nefor.process.run to `rg` (preferred) or `grep` as
--                    fallback. Path defaults to ".". Pure read.
--
-- Layered so an explorer / reviewer agent can investigate the codebase
-- without needing the full `bash` surface (which is a sandbox-escape
-- hatch via shell composition).

local json = nefor.json

local envelope = require("core.envelope")
local emit_as  = envelope.emit_as
local output_dump = require("tool-gate.tool_output_dump")

local SOURCE_NAME = "read-only-tools"

local function emit_ok(firing_id, text, meta)
  local out = tostring(text or "")
  if output_dump.should_dump(out) then
    local summary, _, err = output_dump.dump(
      nil,
      firing_id,
      out,
      meta
    )
    if summary then
      out = summary
    elseif nefor.log then
      nefor.log.warn("read-only-tools: dump failed; forwarding original output", {
        tool_id = firing_id,
        error = err,
      })
    end
  end
  emit_as(SOURCE_NAME, nil, {
    kind   = "tool.result",
    id     = firing_id,
    output = { text = out },
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
  emit_ok(firing_id, table.concat(lines, "\n"), {
    tool = "list_dir",
    args = args,
  })
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

local function bool_arg(v)
  if v == true then return true end
  if v == false or v == nil then return false end
  if v == "true" then return true end
  if v == "false" then return false end
  return nil
end

local function append_flag(argv, cond, flag)
  if cond then argv[#argv + 1] = flag end
end

local function validate_search_args(args)
  local allowed = {
    pattern = true,
    query = true,
    text = true,
    path = true,
    max_results = true,
    case_insensitive = true,
    files_only = true,
  }
  for k, _ in pairs(args or {}) do
    if not allowed[k] then
      return "search_text: unsupported arg `" .. tostring(k) .. "`"
    end
  end
  return nil
end

local function tool_search_text(firing_id, args)
  args = args or {}
  local arg_err = validate_search_args(args)
  if arg_err then
    emit_err(firing_id, arg_err)
    return
  end

  local pattern = args.pattern or args.query or args.text
  if type(pattern) ~= "string" or #pattern == 0 then
    emit_err(firing_id, "search_text: args.pattern must be a non-empty string")
    return
  end
  local path = (type(args.path) == "string" and #args.path > 0) and args.path or "."
  local cap  = tonumber(args.max_results) or 100
  if cap < 1 then cap = 1 end
  if cap > 500 then cap = 500 end
  local case_insensitive = bool_arg(args.case_insensitive)
  if case_insensitive == nil then
    emit_err(firing_id, "search_text: args.case_insensitive must be boolean")
    return
  end
  local files_only = bool_arg(args.files_only)
  if files_only == nil then
    emit_err(firing_id, "search_text: args.files_only must be boolean")
    return
  end

  local backend = resolve_search_cmd()
  local argv
  if backend == "rg" then
    argv = {}
    if files_only then
      argv[#argv + 1] = "-l"
    else
      argv[#argv + 1] = "-n"
      argv[#argv + 1] = "--max-count"
      argv[#argv + 1] = tostring(cap)
    end
    argv[#argv + 1] = "--color=never"
    append_flag(argv, case_insensitive, "-i")
    argv[#argv + 1] = "--"
    argv[#argv + 1] = pattern
    argv[#argv + 1] = path
  else
    argv = { files_only and "-rl" or "-rn", "--color=never" }
    append_flag(argv, case_insensitive, "-i")
    argv[#argv + 1] = "--"
    argv[#argv + 1] = pattern
    argv[#argv + 1] = path
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
    emit_ok(firing_id, "(no matches)", {
      tool = "search_text",
      args = args,
    })
    return
  end
  -- Truncate to cap lines defensively. `rg --max-count` is per-file,
  -- not total, and files-only mode has no backend-side total cap.
  local truncated = {}
  local n = 0
  for line in stdout:gmatch("[^\n]+") do
    n = n + 1
    if n > cap then
      truncated[#truncated + 1] = "[...truncated, raise max_results]"
      break
    end
    truncated[#truncated + 1] = line
  end
  emit_ok(firing_id, table.concat(truncated, "\n"), {
    tool = "search_text",
    args = args,
  })
end

local TOOL_HANDLERS = {
  list_dir    = tool_list_dir,
  search_text = tool_search_text,
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
    },
    {
      name = "search_text",
      description =
        "Search for a regex pattern in files under a path (recursively). " ..
        "Returns matching lines as `path:line:match` or only matching " ..
        "paths when files_only=true. Uses ripgrep when available, " ..
        "POSIX grep otherwise. Read-only.",
      parameters = {
        type = "object",
        properties = {
          pattern = { type = "string",
                      description = "Regex pattern (ERE / rg syntax)." },
          query = { type = "string",
                    description = "Alias for pattern." },
          path = { type = "string",
                   description = "Search root (file or directory). Defaults to '.'." },
          max_results = { type = "integer",
                          description = "Cap on returned lines/files (default 100, max 500)." },
          case_insensitive = { type = "boolean",
                               description = "Use case-insensitive matching." },
          files_only = { type = "boolean",
                         description = "Return only matching file paths." },
        },
        required = { "pattern" },
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
