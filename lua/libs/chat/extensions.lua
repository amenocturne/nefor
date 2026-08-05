-- Optional config-facing additions to the canonical chat consumer.
--
-- Config selects one module through `config.active.chat_extension`. The module
-- may contribute slash commands and presentation, but it never receives the
-- mutable canonical state table. Command handlers get a lazy read-only view
-- and must return state/effects through the helpers supplied by chat.update.

local M = {}
local active = nil
local command_handlers = {}

local function readonly(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local proxy = {}
  seen[value] = proxy
  setmetatable(proxy, {
    __index = function(_, key) return readonly(value[key], seen) end,
    __newindex = function() error("chat extension state is read-only", 2) end,
    __len = function() return #value end,
    __pairs = function()
      local key = nil
      return function()
        key = next(value, key)
        if key == nil then return nil end
        return key, readonly(value[key], seen)
      end
    end,
    __metatable = false,
  })
  return proxy
end

local function copy_array(values)
  local out = {}
  for _, value in ipairs(values or {}) do out[#out + 1] = value end
  return out
end

local function validate_command(command)
  if type(command) ~= "table" or type(command.name) ~= "string"
      or command.name == "" then
    error("chat extension commands require a non-empty name")
  end
  if type(command.run) ~= "function" then
    error("chat extension command `" .. command.name .. "` requires run(args, state, api)")
  end
end

function M.configure(extension)
  active = extension
  command_handlers = {}
  if extension == nil then return end
  if type(extension) ~= "table" then
    error("config.active.chat_extension must resolve to a table")
  end
  for _, command in ipairs(extension.commands or {}) do
    validate_command(command)
    if command_handlers[command.name] ~= nil then
      error("duplicate chat extension command `" .. command.name .. "`")
    end
    command_handlers[command.name] = command
  end
end

function M.load(config)
  local selected = type(config) == "table" and config.chat_extension or nil
  if type(selected) == "string" and selected ~= "" then selected = require(selected) end
  M.configure(selected)
end

-- Merge extension declarations into the canonical completion registry.
-- A declaration that shares a canonical name must say `extend = true`; it
-- keeps the canonical label/aliases and only adds argument completions.
function M.commands(base)
  local out, by_name = {}, {}
  for _, command in ipairs(base or {}) do
    local copied = {}
    for key, value in pairs(command) do copied[key] = value end
    copied.arg_completions = copy_array(command.arg_completions)
    out[#out + 1] = copied
    by_name[copied.name] = copied
  end
  if active == nil then return out end
  for _, command in ipairs(active.commands or {}) do
    local existing = by_name[command.name]
    if existing ~= nil then
      if command.extend ~= true then
        error("chat extension command `" .. command.name
          .. "` collides with a canonical command; set extend = true")
      end
      for _, argument in ipairs(command.arg_completions or {}) do
        existing.arg_completions[#existing.arg_completions + 1] = argument
      end
    else
      local declaration = {}
      for key, value in pairs(command) do
        if key ~= "run" and key ~= "extend" then declaration[key] = value end
      end
      out[#out + 1] = declaration
      by_name[declaration.name] = declaration
    end
  end
  return out
end

function M.handle_command(name, args, state, api)
  local command = command_handlers[name]
  if command == nil then return nil end
  local next_state, effects = command.run(args, readonly(state), api)
  if next_state == nil then return nil end
  return next_state, effects or {}
end

function M.initial_patch(state)
  if active == nil or type(active.initial_state) ~= "function" then return nil end
  local patch = active.initial_state(readonly(state))
  if patch ~= nil and type(patch) ~= "table" then
    error("chat extension initial_state must return a table patch or nil")
  end
  return patch
end

function M.status_segments(state)
  if active == nil or type(active.status_segments) ~= "function" then return {} end
  local segments = active.status_segments(readonly(state))
  if segments == nil then return {} end
  if type(segments) ~= "table" then
    error("chat extension status_segments must return an array or nil")
  end
  return segments
end

function M.input_border_style(state, focused, canonical)
  if active == nil or type(active.input_border_style) ~= "function" then
    return canonical
  end
  return active.input_border_style(readonly(state), focused, canonical) or canonical
end

return M
