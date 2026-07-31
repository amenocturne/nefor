-- Bounded startup barrier for prompt-bearing surfaces.
--
-- A final plugin hello only proves spawn ordering. Readiness instead requires
-- explicit liveness from every actor the first turn needs and the complete
-- public tool catalog assembled by tool-gate.

local M = {}

local function set(values)
  local out = {}
  for _, value in ipairs(values or {}) do out[value] = true end
  return out
end

local function sorted_keys(values)
  local out = {}
  for value, present in pairs(values) do
    if present then out[#out + 1] = value end
  end
  table.sort(out)
  return out
end

local function join(values)
  return #values == 0 and "none" or table.concat(values, ", ")
end

local function new_barrier(opts)
  opts = opts or {}
  local required_plugins = set(opts.required_plugins)
  local required_tools = set(opts.required_tools)
  local tool_sources = opts.tool_sources or {}
  local live_plugins = {}
  local advertised_tools = {}
  local settled = false

  local function missing(required, observed)
    local out = {}
    for name in pairs(required) do
      if not observed[name] then out[#out + 1] = name end
    end
    table.sort(out)
    return out
  end

  local function missing_source_hints(missing_tools)
    local missing_set = set(missing_tools)
    local hints = {}
    for source, tools in pairs(tool_sources) do
      local source_missing = {}
      for _, name in ipairs(tools) do
        if missing_set[name] then source_missing[#source_missing + 1] = name end
      end
      if #source_missing > 0 then
        table.sort(source_missing)
        hints[#hints + 1] = source .. " (" .. table.concat(source_missing, ", ") .. ")"
      end
    end
    table.sort(hints)
    return hints
  end

  local function snapshot()
    local plugins = missing(required_plugins, live_plugins)
    local tools = missing(required_tools, advertised_tools)
    return {
      ready = #plugins == 0 and #tools == 0,
      missing_plugins = plugins,
      missing_tools = tools,
      missing_sources = missing_source_hints(tools),
    }
  end

  local function maybe_ready()
    if settled then return end
    local state = snapshot()
    if not state.ready then return end
    settled = true
    if opts.on_ready then opts.on_ready() end
  end

  local function observe(body, from)
    if settled or type(body) ~= "table" then return end
    local kind = body.kind
    if type(kind) ~= "string" then return end

    local plugin = kind:match("^(.+)%.hello$")
    if plugin and required_plugins[plugin] then live_plugins[plugin] = true end
    -- Provider compositors intentionally translate private `<name>.hello`
    -- into the public model acknowledgement. Its provider field remains the
    -- stable liveness identity for both real and scripted providers.
    if kind == "chat.model.set_ack" and type(body.provider) == "string"
        and required_plugins[body.provider] then
      live_plugins[body.provider] = true
    end

    if kind == "tool.register" and (from == nil or from == "tool-gate") then
      local replacement = {}
      for _, schema in ipairs(body.tools or {}) do
        if type(schema) == "table" and type(schema.name) == "string" then
          replacement[schema.name] = true
        end
      end
      advertised_tools = replacement
    end
    maybe_ready()
  end

  local function timeout()
    if settled then return end
    settled = true
    local state = snapshot()
    local message = "startup readiness timed out; missing plugin hello(s): "
      .. join(state.missing_plugins)
      .. "; missing required tool(s): " .. join(state.missing_tools)
    if #state.missing_sources > 0 then
      message = message .. "; expected advertisement source(s): "
        .. table.concat(state.missing_sources, "; ")
    end
    message = message
      .. ". Check that the listed plugin binaries are installed and that each source advertises to tool-gate."
    if opts.on_error then opts.on_error(message, state) end
  end

  return {
    observe = observe,
    timeout = timeout,
    snapshot = snapshot,
    settled = function() return settled end,
  }
end

function M.wait(opts)
  opts = opts or {}
  local barrier = new_barrier(opts)

  nefor.bus.on_event("*", function(entry)
    local payload = type(entry) == "table" and entry.payload or nil
    if type(payload) ~= "string" then return end
    local ok, env = pcall(nefor.json.decode, payload)
    if not ok or type(env) ~= "table" then return end
    barrier.observe(env.body, env.from)
  end)

  local timeout_ms = tonumber(opts.timeout_ms) or 10000
  local seconds = math.max(timeout_ms, 1) / 1000
  local timer
  timer = nefor.process.spawn {
    cmd = "sh",
    args = { "-c", "sleep " .. tostring(seconds) },
    on_exit = function() barrier.timeout() end,
  }
  return barrier, timer
end

M._new = new_barrier
M._sorted_keys = sorted_keys

return M
