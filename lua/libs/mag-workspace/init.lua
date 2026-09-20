-- lua/libs/mag-workspace/init.lua — MAG workspace management and preview formatting.
--
-- Provides two things:
--   1. Workspace lifecycle: init an empty per-session MAG source directory.
--   2. Preview formatting: render a graph modification (the shape the
--      mag plugin replies with on `mag.loaded`) into a human-readable
--      string the lead can inspect before executing.
--
-- Compilation itself lives in the mag plugin: the lead emits `mag.load`
-- and reads the modification off the `mag.loaded` reply
-- (examples/nefor-agent/lead-workflow/init.lua). The `mag` CLI binary remains a dev
-- tool for humans; nothing here shells out to it.

local M = {}
local configured_sessions_root = nil

function M.configure(options)
  options = options or {}
  configured_sessions_root = options.sessions_root or configured_sessions_root
end

-- Explicit per-consumer opt-in; the composition owns storage selection.
function M.project_build_options(options)
  if options == nil or options == false then return nil end
  assert(type(options) == "table", "project_build must be a table or false")
  assert(type(options.cache_dir) == "string" and options.cache_dir:sub(1, 1) == "/",
    "project_build.cache_dir must be an absolute path")
  assert(options.no_cache == nil or type(options.no_cache) == "boolean",
    "project_build.no_cache must be a boolean")
  return { cache_dir = options.cache_dir, no_cache = options.no_cache or false }
end

function M.compile_request(id, project_root, entry, module_roots, project_build)
  local request = { id = id, entry = entry, module_roots = module_roots }
  if project_build then
    request.kind = "mag.build"
    request.project_root = project_root
    request.cache_dir = project_build.cache_dir
    request.no_cache = project_build.no_cache
  else
    request.kind = "mag.load"
    request.source_dir = project_root
  end
  return request
end

local function sh_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function sessions_root()
  return configured_sessions_root
end

local function mkdir_p(path)
  if nefor and nefor.fs and type(nefor.fs.mkdir_p) == "function" then
    local ok = pcall(nefor.fs.mkdir_p, path)
    if ok then return true end
  end
  local ok = os.execute("mkdir -p " .. sh_quote(path) .. " >/dev/null 2>&1")
  return ok == true or ok == 0
end

local function exists(path)
  if nefor and nefor.fs and type(nefor.fs.exists) == "function" then
    return nefor.fs.exists(path)
  end
  local handle = io.open(path, "r")
  if handle then handle:close(); return true end
  return false
end

local function is_symlink(path)
  local result = os.execute("test -L " .. sh_quote(path) .. " >/dev/null 2>&1")
  return result == true or result == 0
end

-- Resolve a model-authored source name inside one session workspace. Rejecting
-- symlink components is what makes lexical containment meaningful even when a
-- prior process has written into the workspace.
function M.resolve_file(workspace, file)
  if type(workspace) ~= "string" or workspace:sub(1, 1) ~= "/" then
    return nil, "workspace must be an absolute path"
  end
  if type(file) ~= "string" or file == "" then return nil, "file must be non-empty" end
  if file:sub(1, 1) == "/" then return nil, "absolute paths are not allowed: " .. file end
  local parts = {}
  for part in file:gmatch("[^/]+") do
    if part == "." or part == ".." then
      return nil, "path traversal is not allowed: " .. file
    end
    if part:find("%z") then return nil, "file contains a NUL byte" end
    parts[#parts + 1] = part
  end
  if #parts == 0 or table.concat(parts, "/") ~= file then
    return nil, "file must be a normalized relative path: " .. file
  end
  local current = workspace
  if is_symlink(current) then return nil, "workspace is a symlink" end
  for _, part in ipairs(parts) do
    current = current .. "/" .. part
    if is_symlink(current) then return nil, "symlink paths are not allowed: " .. file end
  end
  return current
end

local function ensure_parent(path)
  local parent = path:match("(.+)/[^/]+$")
  if parent and not mkdir_p(parent) then return nil, "cannot create parent directory" end
  return true
end

local function read_text(path)
  local handle, err = io.open(path, "rb")
  if not handle then return nil, err end
  local value = handle:read("*a")
  handle:close()
  if value:find("%z") then return nil, "file contains binary data" end
  return value
end

local function write_text(path, content, exclusive)
  local ok, err = ensure_parent(path)
  if not ok then return nil, err end
  local write_path = path
  local temp_dir
  if exclusive then
    for _ = 1, 16 do
      local candidate = path .. ".create-" .. tostring(os.time()) .. "-"
        .. tostring(math.random(100000, 999999))
      local made = os.execute("mkdir -m 700 " .. sh_quote(candidate) .. " >/dev/null 2>&1")
      if made == true or made == 0 then temp_dir = candidate; break end
    end
    if not temp_dir then return nil, "cannot create a private temporary directory" end
    write_path = temp_dir .. "/source"
  end
  local handle, open_error = io.open(write_path, "w")
  if not handle then
    if temp_dir then os.execute("rmdir " .. sh_quote(temp_dir) .. " >/dev/null 2>&1") end
    return nil, open_error
  end
  local wrote, write_error = handle:write(content)
  local closed, close_error = handle:close()
  if not wrote or not closed then
    os.remove(write_path)
    if temp_dir then os.execute("rmdir " .. sh_quote(temp_dir) .. " >/dev/null 2>&1") end
    return nil, write_error or close_error
  end
  if exclusive then
    -- POSIX hard-link creation is atomic and refuses an existing destination;
    -- unlike rename it cannot clobber a concurrently created source file.
    local linked = os.execute("ln " .. sh_quote(write_path) .. " " .. sh_quote(path) .. " >/dev/null 2>&1")
    os.remove(write_path)
    os.execute("rmdir " .. sh_quote(temp_dir) .. " >/dev/null 2>&1")
    if linked ~= true and linked ~= 0 then return nil, "file already exists or could not be created" end
  end
  return true
end

function M.write_file(workspace, file, new_string, old_string)
  local path, path_error = M.resolve_file(workspace, file)
  if not path then return nil, path_error end
  if type(new_string) ~= "string" then return nil, "new_string must be a string" end
  if old_string == nil then
    local existed = exists(path)
    local ok, error = write_text(path, new_string, false)
    if not ok then return nil, error end
    return { operation = existed and "overwritten" or "created", source_path = path }
  end
  if type(old_string) ~= "string" or old_string == "" then
    return nil, "old_string must be a non-empty string when present"
  end
  if old_string == new_string then return nil, "old_string and new_string must differ" end
  local content, read_error = read_text(path)
  if not content then return nil, read_error end
  local count, search_at = 0, 1
  while true do
    local start_at, end_at = content:find(old_string, search_at, true)
    if not start_at then break end
    count = count + 1
    search_at = end_at + 1
  end
  if count == 0 then return nil, "old_string was not found in the file" end
  if count > 1 then return nil, "old_string matched multiple locations; provide more context" end
  local start_at, end_at = content:find(old_string, 1, true)
  local replacement = content:sub(1, start_at - 1) .. new_string .. content:sub(end_at + 1)
  local ok, error = write_text(path, replacement, false)
  if not ok then return nil, error end
  return { operation = "edited", source_path = path }
end

function M.create_file(workspace, file, content)
  local path, path_error = M.resolve_file(workspace, file)
  if not path then return nil, path_error end
  if exists(path) then
    return nil, "file already exists: " .. file ..
      "; omit content to use it, or modify it with mag-write-file before applying"
  end
  if type(content) ~= "string" then return nil, "content must be a string" end
  local ok, error = write_text(path, content, true)
  if not ok then return nil, error end
  return { operation = "created", source_path = path }
end

function M.require_file(workspace, file)
  local path, path_error = M.resolve_file(workspace, file)
  if not path then return nil, path_error end
  if not exists(path) then return nil, "file does not exist: " .. file end
  return path
end

-- Render the authored logical node hierarchy. The compiler owns these paths;
-- actor ids and lowered routes are intentionally absent from the model view.
function M.workflow_tree(descriptors)
  if type(descriptors) ~= "table" or #descriptors == 0 then return "(empty workflow)" end
  local lines, seen = {}, {}
  local function key(path, length)
    local parts = {}
    for index = 1, length or #path do parts[#parts + 1] = tostring(path[index]) end
    return table.concat(parts, "\0")
  end
  for _, descriptor in ipairs(descriptors) do
    local path = type(descriptor) == "table" and descriptor.path or nil
    if type(path) == "table" and #path > 0 then
      for length = 1, #path do
        local path_key = key(path, length)
        if not seen[path_key] then
          seen[path_key] = true
          lines[#lines + 1] = string.rep("  ", length - 1) .. "- " .. tostring(path[length])
        end
      end
    end
  end
  return #lines > 0 and table.concat(lines, "\n") or "(empty workflow)"
end

-- Get the MAG workspace directory for a session.
function M.workspace_dir(session_id)
  local root = sessions_root()
  if not root then return nil end
  return root .. "/" .. session_id .. "/mag"
end

-- Initialize an empty writable workspace. Canonical and config-owned module
-- roots stay where the package/config materialized them and are supplied to
-- the compiler explicitly; copying them here would create stale session state.
-- Returns the workspace path on success, nil + error on failure.
function M.init_workspace(session_id, _config_dir)
  local ws = M.workspace_dir(session_id)
  if not ws then return nil, "no data root available" end

  if not mkdir_p(ws) then
    return nil, "failed to create workspace: " .. ws
  end

  -- noclobber also protects an authored manifest created concurrently. Existing
  -- content (even invalid TOML) belongs to the author, not initialization.
  local manifest = sh_quote(ws .. "/mag.toml")
  local ok = os.execute("test -e " .. manifest
    .. " || (set -C; printf 'version = 1\\n' > " .. manifest .. ")")
  if ok ~= true and ok ~= 0 then
    return nil, "failed to initialize project manifest: " .. ws .. "/mag.toml"
  end
  return ws, nil
end

-- Render one param value compactly: quoted strings (truncated), inline
-- arrays, `{…}` for nested maps.
local MAX_STR = 48

local function format_value(value)
  local t = type(value)
  if t == "string" then
    local s = value
    if #s > MAX_STR then s = s:sub(1, MAX_STR - 1) .. "…" end
    return string.format("%q", s)
  end
  if t == "table" then
    -- Array: render inline. Map: elide (params summaries stay one line).
    if #value > 0 or next(value) == nil then
      local parts = {}
      for _, v in ipairs(value) do parts[#parts + 1] = tostring(v) end
      return "[" .. table.concat(parts, ", ") .. "]"
    end
    return "{…}"
  end
  return tostring(value)
end

local function format_params(params)
  if type(params) ~= "table" or next(params) == nil then return "" end
  local keys = {}
  for k in pairs(params) do keys[#keys + 1] = tostring(k) end
  table.sort(keys)
  local parts = {}
  for _, k in ipairs(keys) do
    parts[#parts + 1] = k .. ": " .. format_value(params[k])
  end
  return " {" .. table.concat(parts, ", ") .. "}"
end

local function initial_actor_address(id)
  id = tostring(id or "")
  return "actor:" .. tostring(#id) .. ":" .. id
end

local function template_actor_address(operation_id, slot)
  operation_id, slot = tostring(operation_id or ""), tostring(slot or "")
  return "operation:" .. tostring(#operation_id) .. ":" .. operation_id
    .. ":template:" .. tostring(#slot) .. ":" .. slot
end

local function copy(value)
  if type(value) ~= "table" then return value end
  local out = {}
  for key, child in pairs(value) do out[key] = copy(child) end
  return out
end

local function exact_fields(value, required, context)
  if type(value) ~= "table" then return nil, context .. " must be an object" end
  for key in pairs(value) do
    if not required[key] then return nil, context .. " has unknown field " .. tostring(key) end
  end
  for key in pairs(required) do
    if value[key] == nil then return nil, context .. " requires " .. key end
  end
  return true
end

local function exact_modification(value, fields, context)
  if type(value) ~= "table" then return nil, context .. " must be an object" end
  for key in pairs(value) do
    if not fields[key] then return nil, context .. " has unknown field " .. tostring(key) end
  end
  for key in pairs(fields) do
    if value[key] == nil then return nil, context .. " requires " .. key end
  end
  return copy(value)
end

local function unpack(value, context)
  local fields = 0
  if type(value) == "table" then
    for _ in pairs(value) do fields = fields + 1 end
  end
  if fields ~= 2 or value["$mag"] ~= "packed-value" or value.value == nil then
    return nil, tostring(context) .. " must be a compiler-owned packed value"
  end
  return copy(value.value)
end

local function validate_boundary(value, context)
  local ok, err = exact_fields(value,
    { type = true, type_id = true, leaves = true, through = true }, context)
  if not ok then return nil, err end
  if type(value.type_id) ~= "string" or type(value.leaves) ~= "table"
      or type(value.through) ~= "table" then
    return nil, context .. " is malformed"
  end
  for index, leaf in ipairs(value.leaves) do
    local leaf_ok, leaf_err = exact_fields(leaf, { port = true, steps = true },
      context .. ".leaves[" .. index .. "]")
    if not leaf_ok then return nil, leaf_err end
  end
  for index, flow in ipairs(value.through) do
    local flow_ok, flow_err = exact_fields(flow, { steps = true },
      context .. ".through[" .. index .. "]")
    if not flow_ok then return nil, flow_err end
  end
  return true
end

local function unpack_template_payload(payload, context)
  local ok, err = exact_fields(payload, { constructor = true, value = true }, context)
  if not ok then return nil, err end
  if payload.constructor == "Static" then
    local value, unpack_error = unpack(payload.value, context .. ".value")
    if unpack_error then return nil, unpack_error end
    return { constructor = "Static", value = value }
  end
  if payload.constructor == "Expression" then
    if type(payload.value) ~= "string" then
      return nil, context .. ".value must be an expression reference string"
    end
    return copy(payload)
  end
  return nil, context .. " has unknown TemplatePayload constructor " .. tostring(payload.constructor)
end

local function unpack_modification(modification, context, decode_content)
  decode_content = decode_content or unpack
  local materialized = copy(modification)
  for index, actor in ipairs(materialized.actors or {}) do
    local value, err = unpack(actor.params, context .. ".actors[" .. index .. "].params")
    if err then return nil, err end
    actor.params = value
  end
  for index, message in ipairs(materialized.messages or {}) do
    local value, err = decode_content(message.content, context .. ".messages[" .. index .. "].content")
    if err then return nil, err end
    message.content = value
  end
  return materialized
end

local function unpack_operations(operations)
  local materialized = copy(operations)
  for operation_index, operation in ipairs(materialized) do
    local ok, operation_error = exact_fields(operation, {
      id = true, on = true, captures = true, expressions = true, template = true,
    }, "program.operations[" .. operation_index .. "]")
    if not ok then return nil, operation_error end
    for capture_id, capture in pairs(operation.captures or {}) do
      local value, err = unpack(capture.value, "program.operations[" .. operation_index
        .. "].captures." .. tostring(capture_id) .. ".value")
      if err then return nil, err end
      capture.value = value
    end
    local template, err = unpack_modification(operation.template or {},
      "program.operations[" .. operation_index .. "].template", unpack_template_payload)
    if not template then return nil, err end
    operation.template = template
  end
  return materialized
end

function M.decode_artifact(artifact)
  if type(artifact) ~= "table" or artifact.format ~= "nefor.mag" or artifact.version ~= 4 then
    return nil, "artifact must be a nefor.mag version 4 envelope"
  end
  if artifact.kind == "program" then
    local ok, envelope_error = exact_fields(artifact,
      { format = true, version = true, kind = true, program = true }, "program envelope")
    if not ok then return nil, envelope_error end
    ok, envelope_error = exact_fields(artifact.program,
      { initial = true, operations = true }, "program payload")
    if not ok or type(artifact.program.operations) ~= "table" then
      return nil, envelope_error or "program.operations must be a list"
    end
    local initial, initial_error = exact_modification(artifact.program.initial, {
      types = true, actors = true, routes = true,
      messages = true, nodes = true, kills = true, result = true,
    }, "program.initial")
    if not initial then return nil, initial_error end
    local boundary_ok, boundary_error = validate_boundary(initial.result and initial.result.from,
      "program.initial.result.from")
    if not boundary_ok then return nil, boundary_error end
    local modification, modification_error = unpack_modification(initial, "program.initial")
    if not modification then return nil, modification_error end
    local operations, operations_error = unpack_operations(artifact.program.operations)
    if not operations then return nil, operations_error end
    return { kind = "program", modification = modification, operations = operations }
  end
  if artifact.kind == "delta" then
    local ok, envelope_error = exact_fields(artifact,
      { format = true, version = true, kind = true, delta = true }, "delta envelope")
    if not ok then return nil, envelope_error end
    local delta, delta_error = exact_modification(artifact.delta, {
      types = true, actors = true, routes = true,
      messages = true, nodes = true, kills = true,
    }, "delta")
    if not delta then return nil, delta_error end
    local modification, modification_error = unpack_modification(delta, "delta")
    if not modification then return nil, modification_error end
    return { kind = "delta", modification = modification, operations = {} }
  end
  return nil, "artifact envelope has an unsupported or malformed variant"
end

function M.actor_inventory(decoded)
  local entries = {}
  for _, actor in ipairs(decoded.modification.actors or {}) do
    entries[#entries + 1] = {
      address = initial_actor_address(actor.id), actor = actor, kind = "initial",
    }
  end
  for operation_index, operation in ipairs(decoded.operations or {}) do
    local template = type(operation) == "table" and operation.template or nil
    for actor_index, actor in ipairs(type(template) == "table" and template.actors or {}) do
      entries[#entries + 1] = {
        address = template_actor_address(operation.id, actor.slot), actor = actor,
        kind = "template", operation = operation, operation_index = operation_index,
        actor_index = actor_index,
      }
    end
  end
  return entries
end

M.initial_actor_address = initial_actor_address
M.template_actor_address = template_actor_address

local function template_ref(ref)
  local value = type(ref) == "table" and (ref.value or ref) or {}
  if value.slot ~= nil then return "slot:" .. tostring(value.slot) end
  if value.id ~= nil then return "existing:" .. tostring(value.id) end
  return "<invalid-ref>"
end

local function template_port(port)
  if type(port) ~= "table" then return "<invalid-port>" end
  return template_ref(port.endpoint) .. "/" .. tostring(port.wire)
end

local function endpoint_ref(endpoint)
  if type(endpoint) ~= "table" or endpoint.constructor ~= "ActorEndpoint" then
    return "<invalid-actor-endpoint>"
  end
  local value = type(endpoint.value) == "table" and endpoint.value or {}
  return "actor:" .. tostring(value.id or "<invalid-id>")
end

local function port_ref(port)
  if type(port) ~= "table" then return "<invalid-port>" end
  return endpoint_ref(port.endpoint) .. "/" .. tostring(port.wire)
end

local function append_actor(lines, prefix, actor)
  lines[#lines + 1] = string.format("  %s%s (%s)%s", prefix or "",
    tostring(actor.id or actor.slot), tostring(actor.factory), format_params(actor.params))
end

local function format_transform(transform)
  if type(transform) ~= "table" then return tostring(transform) end
  local constructor = tostring(transform.constructor or "transform")
  local value = transform.value
  if type(value) ~= "table" then return constructor end
  local fields = {}
  for key, item in pairs(value) do fields[#fields + 1] = tostring(key) .. "=" .. format_value(item) end
  table.sort(fields)
  return constructor .. (#fields > 0 and " {" .. table.concat(fields, ", ") .. "}" or "")
end

local function append_transforms(lines, prefix, transforms)
  if type(transforms) ~= "table" or #transforms == 0 then
    lines[#lines + 1] = prefix .. " transforms: (identity)"
    return
  end
  for index, transform in ipairs(transforms) do
    lines[#lines + 1] = string.format("%s transform[%d]: %s", prefix, index,
      format_transform(transform))
  end
end

local function append_boundary(lines, boundary)
  if type(boundary) ~= "table" then return end
  lines[#lines + 1] = ""
  lines[#lines + 1] = "Result boundary: " .. tostring(boundary.type_id or "<unknown type>")
  lines[#lines + 1] = string.format("  leaves: %d, through: %d",
    #(boundary.leaves or {}), #(boundary.through or {}))
  for index, leaf in ipairs(boundary.leaves or {}) do
    lines[#lines + 1] = string.format("  leaf[%d]: %s", index, port_ref(leaf.port))
    append_transforms(lines, "    ", leaf.steps)
  end
  for index, flow in ipairs(boundary.through or {}) do
    lines[#lines + 1] = string.format("  through[%d]:", index)
    append_transforms(lines, "    ", flow.steps)
  end
end

-- Format a versioned immutable program or delta envelope without mutating it.
function M.preview(artifact, hash, factories)
  local decoded, error = M.decode_artifact(artifact)
  if not decoded then return "(invalid MAG artifact: " .. tostring(error) .. ")" end
  local modification = decoded.modification
  local actors = modification.actors or {}
  local routes, messages = modification.routes or {}, modification.messages or {}
  local operations = decoded.operations or {}
  local lines = {}
  lines[#lines + 1] = string.format(
    "%s envelope: %d actors, %d routes, %d messages, %d operations",
    decoded.kind == "program" and "Program" or "Delta",
    #actors, #routes, #messages, #operations)
  lines[#lines + 1] = "Hash: " .. tostring(hash)
  lines[#lines + 1] = ""
  lines[#lines + 1] = "Initial actors:"
  for _, actor in ipairs(actors) do append_actor(lines, "", actor) end
  if #routes > 0 then
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Routes:"
    for index, route in ipairs(routes) do
      lines[#lines + 1] = string.format("  route[%d]: %s -> %s",
        index, port_ref(route.from), port_ref(route.to))
      append_transforms(lines, "    ", route.transforms)
    end
  end

  for index, operation in ipairs(operations) do
    local template = type(operation.template) == "table" and operation.template or {}
    lines[#lines + 1] = ""
    lines[#lines + 1] = string.format("Operation %d: %s on %s", index,
      tostring(operation.id), template_port(operation.on))
    lines[#lines + 1] = string.format("  Template: %d actors, %d routes, %d messages",
      #(template.actors or {}), #(template.routes or {}), #(template.messages or {}))
    for _, actor in ipairs(template.actors or {}) do
      append_actor(lines, "[" .. template_actor_address(operation.id, actor.slot) .. "] ", actor)
    end
    for route_index, route in ipairs(template.routes or {}) do
      lines[#lines + 1] = string.format("    route[%d]: %s -> %s",
        route_index, template_port(route.from), template_port(route.to))
      append_transforms(lines, "      ", route.transforms)
    end
    for message_index, message in ipairs(template.messages or {}) do
      lines[#lines + 1] = string.format("    message[%d] -> %s",
        message_index, template_port(message.to))
      append_transforms(lines, "      ", message.transforms)
    end
  end

  if #messages > 0 then
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Initial messages:"
    for index, msg in ipairs(messages) do
      local kind = type(msg.content) == "table" and msg.content.kind or nil
      lines[#lines + 1] = string.format("  message[%d] -> %s (%s)", index,
        port_ref(msg.to), tostring(kind or "message"))
      append_transforms(lines, "    ", msg.transforms)
    end
  end
  local result = type(modification.result) == "table" and modification.result.from or nil
  append_boundary(lines, result)
  if type(factories) == "table" and #factories > 0 then
    local names = {}
    for _, factory in ipairs(factories) do names[#names + 1] = tostring(factory) end
    table.sort(names)
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Registry factories: " .. table.concat(names, ", ")
  end
  return table.concat(lines, "\n")
end

return M
