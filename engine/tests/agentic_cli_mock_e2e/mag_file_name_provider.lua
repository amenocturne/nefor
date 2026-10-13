-- Provider behavior for the MAG file-name case of agentic_cli_mock_e2e.rs.
-- The lead calls `mag-apply` once with a file name and the source a model
-- sent in a recorded session (the test substitutes it for the placeholder),
-- then calls each MAG source tool with a file that has no name before its
-- extension, and reports the results it was given.

local mock = require("libs.mock-provider")

local MAG_FILE = "locate.mag"
local MAG_PROGRAM = [==[__MAG_PROGRAM__]==]

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

-- The calls made after the named apply, in order.
local UNNAMED = {
  { "mag-write-file", { file = ".mag", new_string = MAG_PROGRAM } },
  { "mag-preview", { file = "dir/.mag" } },
  { "mag-apply", { file = ".magl", content = MAG_PROGRAM } },
}

local function respond(request)
  local results = {}
  for _, message in ipairs(request.messages) do
    if message.role == "tool" then results[#results + 1] = text_of(message.content) end
  end
  if #results == 0 then
    return mock.tool_call("mag-apply", { file = MAG_FILE, content = MAG_PROGRAM })
  end
  local nxt = UNNAMED[#results]
  if nxt then return mock.tool_call(nxt[1], nxt[2]) end
  local report = { "APPLIED " .. results[1] }
  for index = 2, #results do report[#report + 1] = "<" .. results[index] .. ">" end
  return mock.text(table.concat(report, "\n"))
end

mock.serve { respond = respond }
