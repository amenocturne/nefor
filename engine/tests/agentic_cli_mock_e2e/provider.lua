-- Provider behavior for agentic_cli_mock_e2e.rs. The test selects this file
-- through NEFOR_MOCK_PROVIDER_SCRIPT; every prompt below is one the test sends.

local mock = require("libs.mock-provider")

local MAG_FILE = "octo-lighthouse.mag"
local OCTOPUS_SYSTEM = "Summarise octopuses in one sentence."
local LIGHTHOUSE_SYSTEM = "Summarise lighthouses in one sentence."
local COMBINE_SYSTEM = "Combine the two summaries above into one paragraph."
local DISPATCH_PROMPT = "summarise octopuses and lighthouses in parallel and combine into one paragraph"

-- A chain rather than a join keeps the run deterministic while still giving
-- the lead one dispatched run with three child agents. The last one authors
-- MAG source of its own, which lands in its workspace, not the lead's.
local MAG_PROGRAM = [=[
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type InvestigationInput {prompt: String}
type OctopusSummary {content: String}
type LighthouseSummary {content: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ResolvedModel(model)
let resolved_model_value = nefor.actors.ResolvedModel {compaction_policy: nefor.actors.disabled_compaction, provider: "mock-plugin", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}

let start = nefor.graph.source("task", InvestigationInput {prompt: "<initial task text>"})
let sx = nefor.actors.agent<nefor.actors.ResolvedModel, InvestigationInput, OctopusSummary>("sx", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Summarise octopuses in one sentence.", toolset: nefor.contracts.Toolset {policy: nefor.contracts.ToolApprovalPolicy.Default, tools: []}})
let sy = nefor.actors.agent<nefor.actors.ResolvedModel, core.types.Result<nefor.contracts.AgentError, OctopusSummary>, LighthouseSummary>("sy", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Summarise lighthouses in one sentence.", toolset: nefor.contracts.Toolset {policy: nefor.contracts.ToolApprovalPolicy.Default, tools: []}})
let combine = nefor.actors.agent<nefor.actors.ResolvedModel, core.types.Result<nefor.contracts.AgentError, LighthouseSummary>, nefor.contracts.TextAnswer>("combine", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Combine the two summaries above into one paragraph.", toolset: nefor.contracts.Toolset {policy: nefor.contracts.ToolApprovalPolicy.ReadOnly, tools: ["mag-write-file", "mag-preview"]}})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, sx), nefor.graph.edge(sx, sy), nefor.graph.edge(sy, combine), nefor.graph.edge(combine, result)])
nefor.artifact.compile(topology)
]=]

local NOTES_FILE = "combine-notes.mag"
local NOTES_PROGRAM = [=[
import core.types.{}
import nefor.actors.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}

type NoteInput {prompt: String}

let exact_model: fn(nefor.actors.ResolvedModel) -> nefor.actors.AuthoredModel = |model| => nefor.actors.AuthoredModel.ResolvedModel(model)
let resolved_model_value = nefor.actors.ResolvedModel {compaction_policy: nefor.actors.disabled_compaction, provider: "mock-plugin", model: "mock-model", reasoning_effort: nefor.actors.reasoning_effort("medium")}

let start = nefor.graph.source("note-task", NoteInput {prompt: "<note>"})
let note = nefor.actors.agent<nefor.actors.ResolvedModel, NoteInput, nefor.contracts.TextAnswer>("note-taker", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Take a note.", tools: [], tool_approval_policy: nefor.contracts.ToolApprovalPolicy.Default})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, note), nefor.graph.edge(note, result)])
nefor.artifact.compile(topology)
]=]

local OCTOPUS_TEXT = "Octopuses are highly intelligent cephalopods known for problem-solving and camouflage."
local LIGHTHOUSE_TEXT = "Lighthouses are coastal towers whose beams guide ships past hazards."
local COMBINED_TEXT = "Octopuses and the steadfast lighthouse are both vigilant sentinels, one beneath the waves and one above them."

local function contains(haystack, needle)
  return type(haystack) == "string" and string.find(haystack, needle, 1, true) ~= nil
end

local function text_of(content)
  if type(content) == "string" then return content end
  return nefor.json.encode(content)
end

-- The typed agents' result types are declared in MAG_PROGRAM above.
local function typed_result(last, value)
  if last.role == "tool" and last.name == "write_output" then
    return mock.tool_call("submit_output", {})
  end
  if last.role == "tool" and last.name == "submit_output" then
    return mock.error("typed result was rejected: " .. text_of(last.content))
  end
  return mock.tool_call("write_output", { new_string = nefor.json.encode(value), validate = true })
end

-- The combining node writes a program into its own workspace, compiles it
-- there, and then asks for the lead's file by name. That name does not exist
-- in its workspace, so it answers only once the lead's file proved unreachable.
local function combine(last)
  if last.role ~= "tool" then
    return mock.tool_call("mag-write-file", { file = NOTES_FILE, new_string = NOTES_PROGRAM })
  end
  local content = text_of(last.content)
  if last.name == "mag-write-file" then
    if not contains(content, "/combine/" .. NOTES_FILE) then
      return mock.error("combine wrote outside its own workspace: " .. content)
    end
    return mock.tool_call("mag-preview", { file = NOTES_FILE })
  end
  if last.name == "mag-preview" then
    if contains(content, "note-taker") then return mock.tool_call("mag-preview", { file = MAG_FILE }) end
    if contains(content, "file does not exist: " .. MAG_FILE) then return mock.text(COMBINED_TEXT) end
  end
  return mock.error("unexpected tool result for combine from " .. tostring(last.name) .. ": " .. content)
end

local function lead_after_tool(tool)
  if tool.name == "read_file" then return mock.text("README: " .. text_of(tool.content)) end
  if tool.name == "mag-write-file" then return mock.tool_call("mag-apply", { file = MAG_FILE }) end
  if tool.name == "mag-apply" then
    -- A fast run completes inside the apply result; otherwise the result
    -- arrives later as a user message.
    if contains(text_of(tool.content), COMBINED_TEXT) then return mock.text(COMBINED_TEXT) end
    return mock.text("Started the MAG run.")
  end
  return mock.error("unexpected tool result from " .. tostring(tool.name))
end

local function respond(request)
  local last = request.messages[#request.messages]
  if contains(request.system, OCTOPUS_SYSTEM) then return typed_result(last, { content = OCTOPUS_TEXT }) end
  if contains(request.system, LIGHTHOUSE_SYSTEM) then return typed_result(last, { content = LIGHTHOUSE_TEXT }) end
  if contains(request.system, COMBINE_SYSTEM) then return combine(last) end

  if last.role == "tool" then return lead_after_tool(last) end
  local prompt = text_of(last.content)
  if contains(prompt, COMBINED_TEXT) then return mock.text(COMBINED_TEXT) end
  if prompt == "Summarise octopuses in one sentence." then return mock.text(OCTOPUS_TEXT) end
  if prompt == "read readme" then return mock.tool_call("read_file", { path = "README.md" }) end
  if prompt == "hello" then return mock.text("hello") end
  if prompt == "fail" then return mock.error("scripted provider failure") end
  if contains(prompt, DISPATCH_PROMPT) then
    return mock.tool_call("mag-write-file", { file = MAG_FILE, new_string = MAG_PROGRAM })
  end
  if contains(prompt, "SLOW_STREAM_REGRESSION_") then
    -- A blocking child process: the provider-death case requires the killed
    -- plugin to own a descendant that the runtime must reap.
    os.execute("sleep 1.2")
    return mock.text("slow regression payload acknowledged")
  end
  return mock.error("agentic_cli_mock_e2e has no scripted answer for: " .. prompt)
end

-- Startup-readiness cases delay or withhold the provider hello.
local hello
if os.getenv("NEFOR_TEST_PROVIDER_HELLO") == "withhold" then hello = false end
if os.getenv("NEFOR_TEST_PROVIDER_HELLO") == "delay" then hello = { delay_ms = 500 } end
mock.serve { respond = respond, hello = hello }
