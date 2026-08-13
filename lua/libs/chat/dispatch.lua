local M = {}

local function group_name(group, index)
  if type(group) == "table" and type(group.name) == "string" and group.name ~= "" then
    return group.name
  end
  return "#" .. tostring(index)
end

function M.combine(groups, options)
  options = options or {}
  local duplicate = options.duplicate or "error"
  if duplicate ~= "error" and duplicate ~= "replace" and duplicate ~= "keep" then
    error("unknown duplicate handler policy: " .. tostring(duplicate))
  end

  local combined = {}
  local owners = {}
  for index, group in ipairs(groups or {}) do
    if type(group) ~= "table" or type(group.handlers) ~= "table" then
      error("handler group " .. group_name(group, index) .. " must provide handlers")
    end
    local owner = group_name(group, index)
    for kind, handler in pairs(group.handlers) do
      if type(kind) ~= "string" or kind == "" or type(handler) ~= "function" then
        error("handler group " .. owner .. " contains an invalid registration")
      end
      if combined[kind] ~= nil then
        if duplicate == "error" then
          error("duplicate handler for " .. kind .. " in " .. owners[kind] .. " and " .. owner)
        elseif duplicate == "keep" then
          goto continue
        end
      end
      combined[kind] = handler
      owners[kind] = owner
      ::continue::
    end
  end
  return combined
end

function M.group(name, handlers)
  return { name = name, handlers = handlers }
end

return M
