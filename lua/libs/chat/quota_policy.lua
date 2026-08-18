-- Explicit provider-quota policy for chat presentation.
--
-- Completion token usage is deliberately outside this policy: it continues to
-- feed context occupancy and operation statistics even when quota surfaces are
-- absent.

local M = {}

function M.surfaces_enabled(usage_config)
  local quota = type(usage_config) == "table" and usage_config.quota or nil
  if quota == nil then return true end
  if quota == "none" then return false end
  error("usage.quota must be `none` when set")
end

function M.filter_commands(commands, usage_config)
  if M.surfaces_enabled(usage_config) then return commands end
  local visible = {}
  for _, command in ipairs(commands or {}) do
    if command.name ~= "usage" then visible[#visible + 1] = command end
  end
  return visible
end

return M
