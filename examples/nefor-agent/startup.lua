local M = {}

local MODES = { safe = true, auto = true, yolo = true }

function M.parse(argv)
  local opts = { session_id = nil, prompt = nil, mode = nil }
  local i = 1
  while i <= #argv do
    local arg = argv[i]
    if arg == "--session" then
      local value = argv[i + 1]
      if type(value) ~= "string" or value == "" then
        error("--session requires a session id")
      end
      opts.session_id = value
      i = i + 2
    elseif arg == "--prompt" then
      local value = argv[i + 1]
      if type(value) ~= "string" or value == "" then
        error("--prompt requires a prompt")
      end
      opts.prompt = value
      i = i + 2
    elseif arg == "--mode" then
      local value = argv[i + 1]
      if type(value) ~= "string" or value == "" or value:sub(1, 2) == "--" then
        error("--mode requires one of: safe, auto, yolo")
      end
      if not MODES[value] then
        error("invalid startup mode: " .. value .. " (expected safe, auto, or yolo)")
      end
      opts.mode = value
      i = i + 2
    elseif arg == "--yolo" then
      opts.mode = "yolo"
      i = i + 1
    else
      error("unknown startup arg: " .. tostring(arg))
    end
  end
  return opts
end

function M.apply_mode(options, agentic_loop)
  if options.mode ~= nil then
    agentic_loop.set_mode(options.mode)
  end
end

return M
