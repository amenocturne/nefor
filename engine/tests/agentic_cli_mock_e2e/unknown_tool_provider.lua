-- Provider behavior for the unknown-tool case of agentic_cli_mock_e2e.rs. The
-- lead and a delegated typed worker each call one tool that exists nowhere for
-- them and one that exists but is outside their allowlist, then report the
-- answers they were given.

local mock = require("libs.mock-provider")

local MAG_FILE = "unknown-tool-probe.mag"
local WORKER_SYSTEM = "UNKNOWN_TOOL_WORKER"

-- `bash` is in the worker's allowlist but no source advertises it, so the
-- worker's model is never offered it.
local MAG_PROGRAM = [=[
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type ProbeInput {prompt: String}
type ProbeReport {content: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ResolvedModel(model)
let resolved_model_value = nefor.actors.ResolvedModel {compaction_policy: nefor.actors.disabled_compaction, provider: "mock-plugin", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}

let start = nefor.graph.source("task", ProbeInput {prompt: "<initial task text>"})
let worker = nefor.actors.agent<nefor.actors.ResolvedModel, ProbeInput, ProbeReport>("worker", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "UNKNOWN_TOOL_WORKER", toolset: nefor.contracts.Toolset {policy: nefor.contracts.ToolApprovalPolicy.Default, tools: ["read_image", "bash", "read_file"]}})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, ProbeReport>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, result)])
nefor.artifact.compile(topology)
]=]

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

-- The answers this conversation was given for its two rejected calls.
local function rejections(request)
  local seen = {}
  for _, message in ipairs(request.messages) do
    if message.role == "tool" and (message.name == "bash" or message.name == "write_file") then
      seen[#seen + 1] = "<" .. text_of(message.content) .. ">"
    end
  end
  return table.concat(seen, " ")
end

local function respond(request)
  local last = request.messages[#request.messages]
  local worker = type(request.system) == "string"
    and string.find(request.system, WORKER_SYSTEM, 1, true) ~= nil

  if last.role ~= "tool" then
    if rejections(request) == "" then return mock.tool_call("bash", { command = "git status" }) end
    -- The delegated run finished after the lead's turn; nothing more to do.
    return mock.text("LEAD " .. rejections(request))
  end
  if last.name == "bash" then
    return mock.tool_call("write_file", { path = "must-not-exist.txt", new_string = "blocked" })
  end

  if worker then
    if last.name == "write_file" then
      return mock.tool_call("write_output", {
        new_string = nefor.json.encode({ content = "WORKER " .. rejections(request) }), validate = true,
      })
    end
    if last.name == "write_output" then return mock.tool_call("submit_output", {}) end
    return mock.error("unexpected worker tool result from " .. tostring(last.name))
  end

  if last.name == "write_file" then
    return mock.tool_call("mag-write-file", { file = MAG_FILE, new_string = MAG_PROGRAM })
  end
  if last.name == "mag-write-file" then return mock.tool_call("mag-apply", { file = MAG_FILE }) end
  if last.name == "mag-apply" then return mock.text("LEAD " .. rejections(request)) end
  return mock.error("unexpected lead tool result from " .. tostring(last.name))
end

mock.serve { respond = respond }
