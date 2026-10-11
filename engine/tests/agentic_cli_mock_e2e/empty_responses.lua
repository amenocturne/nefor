-- Provider behavior for the empty-response cases in agentic_cli_mock_e2e.rs.
-- A response with neither text nor a tool call is `mock.text("")`, optionally
-- with reasoning; which request receives one is decided here, per scenario.

local mock = require("libs.mock-provider")

local MAG_FILE = "empty-workers.mag"
local TYPED_SYSTEM = "Summarise octopuses in one sentence."
local PLAIN_SYSTEM = "Repeat the summary above."
local WORKER_PROMPT = "EMPTY_WORKER_RESPONSES"
local TYPED_TEXT = "Octopuses are intelligent cephalopods."
local PLAIN_TEXT = "The typed worker said octopuses are intelligent cephalopods."

local MAG_PROGRAM = [=[
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type InvestigationInput {prompt: String}
type OctopusSummary {content: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ResolvedModel(model)
let resolved_model_value = nefor.actors.ResolvedModel {provider: "mock-plugin", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}

let start = nefor.graph.source("task", InvestigationInput {prompt: "<initial task text>"})
let typed = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, OctopusSummary>("typed", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Summarise octopuses in one sentence.", tools: [], tool_approval_policy: nefor.contracts.ToolApprovalPolicy.Default})
let plain = nefor.actors.agent<nefor.actors.ResolvedModel, core.types.Result<nefor.contracts.AgentError, OctopusSummary>, nefor.contracts.TextAnswer>("plain", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Repeat the summary above.", tools: [], tool_approval_policy: nefor.contracts.ToolApprovalPolicy.Default})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, typed), nefor.graph.edge(typed, plain), nefor.graph.edge(plain, result)])
nefor.artifact.compile(topology)
]=]

local requests = {}

-- How many requests this scenario has made, this one included.
local function request_number(scenario)
  requests[scenario] = (requests[scenario] or 0) + 1
  return requests[scenario]
end

local function contains(haystack, needle)
  return type(haystack) == "string" and string.find(haystack, needle, 1, true) ~= nil
end

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

-- A retried request must be the original request: the empty attempt, and any
-- reasoning it streamed, may not come back as an assistant message.
local function assistant_leak(request)
  for _, message in ipairs(request.messages) do
    if message.role == "assistant" and (message.tool_calls == nil or #message.tool_calls == 0) then
      return mock.error("an empty assistant message reached the retried request")
    end
  end
  return nil
end

local function typed_worker(request, last)
  if last.role == "tool" and last.name == "write_output" then return mock.tool_call("submit_output", {}) end
  if last.role == "tool" then return mock.error("typed result was rejected: " .. text_of(last.content)) end
  if request_number("typed") == 1 then return mock.text("") end
  return assistant_leak(request)
    or mock.tool_call("write_output", { new_string = nefor.json.encode({ content = TYPED_TEXT }), validate = true })
end

local function plain_worker(request)
  if request_number("plain") == 1 then
    return mock.text("", { reasoning = "The summary says octopuses are" })
  end
  return assistant_leak(request) or mock.text(PLAIN_TEXT)
end

local function lead_after_tool(tool)
  if tool.name == "mag-write-file" then return mock.tool_call("mag-apply", { file = MAG_FILE }) end
  if tool.name == "mag-apply" then
    if contains(text_of(tool.content), PLAIN_TEXT) then return mock.text(PLAIN_TEXT) end
    return mock.text("Started the MAG run.")
  end
  return mock.error("unexpected tool result from " .. tostring(tool.name))
end

mock.serve { respond = function(request)
  local last = request.messages[#request.messages]
  if contains(request.system, TYPED_SYSTEM) then return typed_worker(request, last) end
  if contains(request.system, PLAIN_SYSTEM) then return plain_worker(request) end
  if last.role == "tool" then return lead_after_tool(last) end

  local prompt = text_of(last.content)
  if contains(prompt, PLAIN_TEXT) then return mock.text(PLAIN_TEXT) end
  if contains(prompt, "REASONING_ONLY_THEN_ANSWER") then
    if request_number("reasoning-only") == 1 then
      return mock.text("", { reasoning = "The worker also may want to run shell greps. Actually" })
    end
    return assistant_leak(request) or mock.text("recovered answer")
  end
  if contains(prompt, "EMPTY_THEN_ANSWER") then
    if request_number("empty") == 1 then return mock.text("") end
    return assistant_leak(request) or mock.text("recovered answer")
  end
  if contains(prompt, "ALWAYS_EMPTY") then
    request_number("always-empty")
    return mock.text(" \n")
  end
  if contains(prompt, "ORDINARY_ANSWER") then return mock.text("ordinary answer") end
  if contains(prompt, WORKER_PROMPT) then
    return mock.tool_call("mag-write-file", { file = MAG_FILE, new_string = MAG_PROGRAM })
  end
  return mock.error("empty_responses.lua has no scripted answer for: " .. prompt)
end }
