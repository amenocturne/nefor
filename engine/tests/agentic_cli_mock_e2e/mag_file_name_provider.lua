-- Provider behavior for the MAG file-name case of agentic_cli_mock_e2e.rs.
-- The lead calls `mag-apply` once with a file name and the source a model
-- sent in a recorded session (the test substitutes it for the placeholder),
-- then reports the result it was given.

local mock = require("libs.mock-provider")

local MAG_FILE = "locate.mag"
local MAG_PROGRAM = [==[__MAG_PROGRAM__]==]

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

local function respond(request)
  local last = request.messages[#request.messages]
  if last.role ~= "tool" then
    return mock.tool_call("mag-apply", { file = MAG_FILE, content = MAG_PROGRAM })
  end
  if last.name == "mag-apply" then return mock.text("APPLIED " .. text_of(last.content)) end
  return mock.error("unexpected tool result from " .. tostring(last.name))
end

mock.serve { respond = respond }
