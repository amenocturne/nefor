-- Provider behavior for the approval-policy case of agentic_cli_mock_e2e.rs.
-- The lead (Default) applies a workflow with one ReadOnly worker. The worker
-- applies two workflows of its own: one whose command only reads, and one
-- whose command writes a file. It then reports what each apply answered.

local mock = require("libs.mock-provider")

local WORKER_SYSTEM = "READ_ONLY_WORKER"

local OUTER = [=[
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type ProbeInput {prompt: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ResolvedModel(model)
let resolved_model_value = nefor.actors.ResolvedModel {compaction_policy: nefor.actors.disabled_compaction, provider: "mock-plugin", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}

let start = nefor.graph.source("task", ProbeInput {prompt: "<initial task text>"})
let worker = nefor.actors.agent<nefor.actors.ResolvedModel, ProbeInput, nefor.contracts.TextAnswer>("worker", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "READ_ONLY_WORKER", tools: ["mag-apply"], tool_approval_policy: nefor.contracts.ToolApprovalPolicy.ReadOnly})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, worker), nefor.graph.edge(worker, result)])
nefor.artifact.compile(topology)
]=]

local function script_workflow(id, script)
  return ([=[
import nefor.{ShellScriptParams, Timeout, compile_graph, script}

compile_graph(script("%s", ShellScriptParams {
  script: "%s",
  cwd: ".",
  timeout: Timeout.Seconds(30),
}))
]=]):format(id, script)
end

local READS = script_workflow("look", "printf 'read-only-ran'")
local WRITES = script_workflow("leave-a-mark", "touch approval-marker.txt")

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

local function apply_results(request)
  local seen = {}
  for _, message in ipairs(request.messages) do
    if message.role == "tool" and message.name == "mag-apply" then
      seen[#seen + 1] = text_of(message.content)
    end
  end
  return seen
end

local function respond(request)
  local last = request.messages[#request.messages]
  local worker = type(request.system) == "string"
    and string.find(request.system, WORKER_SYSTEM, 1, true) ~= nil
  local results = apply_results(request)

  if worker then
    if #results == 0 then return mock.tool_call("mag-apply", { file = "reads.mag", content = READS }) end
    if #results == 1 then return mock.tool_call("mag-apply", { file = "writes.mag", content = WRITES }) end
    return mock.text("WORKER reads=<" .. results[1] .. "> writes=<" .. results[2] .. ">")
  end

  if last.role == "tool" and last.name == "mag-apply" then return mock.text("LEAD applied") end
  if last.role == "tool" then return mock.error("unexpected lead tool result from " .. tostring(last.name)) end
  if #results == 0 then return mock.tool_call("mag-apply", { file = "outer.mag", content = OUTER }) end
  -- The delegated run reported after the lead's turn; nothing more to do.
  return mock.text("LEAD done")
end

mock.serve { respond = respond }
