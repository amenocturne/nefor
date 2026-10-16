local M = {}

function M.openai(binary, descriptor)
  local command = { binary, "--name", descriptor.name, "--base-url", descriptor.base_url }
  if descriptor.model then
    command[#command + 1] = "--model"
    command[#command + 1] = descriptor.model
  end
  for _, argument in ipairs(descriptor.extra_args or {}) do
    command[#command + 1] = argument
  end
  for _, argument in ipairs(require("openai-provider").retry_args(descriptor.transport_retry)) do
    command[#command + 1] = argument
  end
  return command
end

function M.chatgpt(binary, descriptor)
  local command = { binary, "--name", descriptor.name }
  if descriptor.base_url then
    command[#command + 1] = "--base-url"
    command[#command + 1] = descriptor.base_url
  end
  for _, argument in ipairs(descriptor.extra_args or {}) do
    command[#command + 1] = argument
  end
  return command
end

return M
