-- examples/nefor-agent/mock-provider/init.lua — the starter's interactive mock.
--
-- A canned demo for developers who launch `nefor --config ./examples/nefor-agent`
-- without a real model: each recognised input exercises one part of the chat
-- surface, tool gate, or MAG kernel; anything else prints the help below.
-- The provider wire protocol comes from `libs.mock-provider`; this file holds
-- only the demo's behavior. Tests supply their own behavior beside the test.

local mock = require("libs.mock-provider")

-- Tests that drive this demo through the TUI opt out of pacing.
local fast = os.getenv("NEFOR_TEST_FAST_MOCK") == "1" or os.getenv("NEFOR_TEST_FAST_MOCK") == "true"
local function delay(ms) if fast then return nil end return ms end

-- The demo's MAG program: a chain sx -> sy -> combine. The demo authors the
-- agents' system instructions and result types here, so it recognises its own
-- agents by those instructions and writes their typed results directly.
local MAG_FILE = "octo-lighthouse.mag"
local OCTOPUS_SYSTEM = "Summarise octopuses in one sentence."
local LIGHTHOUSE_SYSTEM = "Summarise lighthouses in one sentence."
local COMBINE_SYSTEM = "Combine the two summaries above into one paragraph."
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
let combine = nefor.actors.agent<nefor.actors.ResolvedModel, core.types.Result<nefor.contracts.AgentError, LighthouseSummary>, nefor.contracts.TextAnswer>("combine", exact_model, nefor.actors.AgentConfig<nefor.actors.ResolvedModel> {model: resolved_model_value, system: "Combine the two summaries above into one paragraph.", toolset: nefor.contracts.Toolset {policy: nefor.contracts.ToolApprovalPolicy.Default, tools: []}})
let result = nefor.graph.output<core.types.Result<nefor.contracts.AgentError, nefor.contracts.TextAnswer>>("result")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, sx), nefor.graph.edge(sx, sy), nefor.graph.edge(sy, combine), nefor.graph.edge(combine, result)])
nefor.artifact.compile(topology)
]=]

local OCTOPUS_TEXT = "Octopuses are highly intelligent invertebrate cephalopods known for problem-solving, dynamic camouflage, and eight prehensile arms lined with chemosensitive suckers."
local LIGHTHOUSE_TEXT = "Lighthouses are tall coastal towers crowned with bright rotating beams that guide ships safely past hazards and into harbours, dating back to the Pharos of Alexandria."
local COMBINED_TEXT = "Octopuses, with their remarkable intelligence and adaptive camouflage, share an unlikely kinship with the steadfast lighthouse — both serve as vigilant sentinels of their respective worlds, the cephalopod beneath the waves and the beacon above them, each watchful in its solitary post."

local RELAY_REASONING = {
  "Reading the MAG run result.\n",
  "It carries a single combined paragraph already, so",
  " I don't need to recompose anything — relaying it",
  " verbatim is the right call.\n",
  "Producing the final answer now.",
}

local THINK_REASONING = {
  "Considering the topic.\n",
  "A deterministic mock has no opinion of its own,",
  " but it can still show the reasoning channel streaming",
  " before the answer.",
}

local BANNER = "**MOCK PROVIDER** -- this is a deterministic test machine, not a real model."

-- The help body deliberately exercises every markdown block kind the chat
-- renderer handles, including emoji and CJK, so broken rendering is visible on
-- the first turn.
local HELP_BODY = table.concat({
  "",
  "# 🤖 Mock test machine",
  "",
  "## What is this?",
  "",
  "You're talking to a **deterministic mock provider** 🧪 — not a real LLM.",
  "Every reply is pattern-matched against the user message, so the same input",
  "always produces the same output. *Useful* for shaking out the chat surface,",
  "tool-gate, kernel-run orchestration, and ~~caching bugs~~ without waiting on",
  "a cloud round-trip.",
  "",
  "Switch to a real model with `/model` 🚀 — the picker lists every connected",
  "provider. Once you pick one, you're talking to it ⚡ at real-LLM latency.",
  "",
  "---",
  "",
  "## Triggers",
  "",
  "| Type | Example | What it exercises |",
  "|------|---------|-------------------|",
  "| mag dispatch | `summarize octopuses and lighthouses` | multi-agent kernel run |",
  "| read_file | `read readme` | 📄 tool-gate allowlist |",
  "| process.exec (pwd) | `what is my cwd` | direct structured process |",
  "| process.exec (ls -la) | `list files` | 📁 direct structured process |",
  "| memory (set) | `the secret key is <v>` | 🔑 store in chat history |",
  "| memory (recall) | `what is the secret key?` | 🔑 history scan |",
  "| translation | `translate hello to japanese` | 🌐 CJK wide-char render |",
  "| streaming | `count to <N>` | 🔢 paced delta emission |",
  "| reasoning | `think out loud about <topic>` (or `reason about <topic>`) | 🤔 reasoning channel |",
  "| error | `fail` | 💥 run-error rendering |",
  "",
  "### 1. mag dispatch",
  "",
  "Writes a canned MAG program to the session workspace and applies it",
  "on the actor kernel: summarise → summarise → combine → sink. Two",
  "tool calls back to back:",
  "",
  "```json",
  "{ \"name\": \"mag-write-file\", \"arguments\": { \"file\": \"octo-lighthouse.mag\", \"new_string\": \"…\" } }",
  "{ \"name\": \"mag-apply\", \"arguments\": { \"file\": \"octo-lighthouse.mag\" } }",
  "```",
  "",
  "### 2. Tool calls",
  "",
  "- 📄 `read readme` — uses the `read_file` tool to fetch `README.md`",
  "  (requires `read_file` on the tool-gate allowlist, or auto)",
  "- `what is my cwd` — uses `process.exec`",
  "- 📁 `list files` — uses `process.exec`",
  "",
  "### 3. Memory",
  "",
  "1. Set: `the secret key is <value>` 🔑 — value enters chat history",
  "2. Recall: `what is the secret key?` — scans history for a prior set",
  "",
  "> Bilingual example: try `translate hello to japanese` — the help",
  "> intentionally includes CJK like 「こんにちは」 / 你好 to exercise",
  "> wide-character rendering in the markdown renderer.",
  "",
  "### 4. Streaming + reasoning",
  "",
  "- 🔢 `count to <N>` — streams `1, 2, 3, …, N` (capped at 50)",
  "- 🤔 `think out loud about <topic>` — emits `reasoning_delta` chunks",
  "  before the final text",
  "",
  "### 5. Error path",
  "",
  "- 💥 `fail` — returns a provider error to exercise the run-error",
  "  rendering path",
  "",
  "---",
  "",
  "*Any other input prints this help.* This provider is the dev surface for",
  "***fast***, ***deterministic*** smoke testing.",
}, "\n")

local function contains(haystack, needle)
  return type(haystack) == "string" and string.find(haystack, needle, 1, true) ~= nil
end

local function text_of(content)
  if type(content) == "string" then return content end
  if type(content) == "table" then
    if type(content.message) == "string" then return content.message end
    return nefor.json.encode(content)
  end
  return tostring(content)
end

-- Truncate to `n` codepoints; byte slicing could split a multibyte sequence.
local function utf8_truncate(s, n)
  local end_byte = utf8.offset(s, n + 1)
  return end_byte and string.sub(s, 1, end_byte - 1) or s
end

local function latest_user(messages)
  for i = #messages, 1, -1 do
    if messages[i].role == "user" then return text_of(messages[i].content), i end
  end
end

-- A typed demo agent writes its result, validates it, then submits.
local function typed_result(last, value)
  if last and last.role == "tool" and last.name == "write_output" then
    local receipt = last.content
    if type(receipt) == "string" then
      local ok, decoded = pcall(nefor.json.decode, receipt)
      receipt = ok and decoded or nil
    end
    if type(receipt) == "table" and type(receipt.validation) == "table"
        and receipt.validation.status == "valid" then
      return mock.tool_call("submit_output", {})
    end
  end
  if last and last.role == "tool" and last.name == "submit_output" then
    return mock.error("demo typed result was rejected: " .. text_of(last.content))
  end
  return mock.tool_call("write_output", { new_string = nefor.json.encode(value), validate = true },
    { delay_ms = delay(5000) })
end

local function demo_agent(request)
  local last = request.messages[#request.messages]
  if contains(request.system, OCTOPUS_SYSTEM) then return typed_result(last, { content = OCTOPUS_TEXT }) end
  if contains(request.system, LIGHTHOUSE_SYSTEM) then return typed_result(last, { content = LIGHTHOUSE_TEXT }) end
  if contains(request.system, COMBINE_SYSTEM) then return mock.text(COMBINED_TEXT, { delay_ms = delay(5000) }) end
end

local function after_tool(tool, user)
  local output = text_of(tool.content)
  local low = string.lower(user or "")
  if tool.name == "mag-write-file" then
    return mock.tool_call("mag-apply", { file = MAG_FILE })
  end
  if tool.name == "mag-apply" then
    if contains(output, COMBINED_TEXT) then
      return mock.text(COMBINED_TEXT, { reasoning = RELAY_REASONING })
    end
    return mock.text("Started the MAG run; I'll relay the results when they arrive.")
  end
  if tool.name == "read_file" then
    return mock.text(string.format("Tool returned README.md (length: %d chars). Snippet: %s...",
      #output, utf8_truncate(output, 200)))
  end
  if tool.name == "process.exec" and contains(low, "list files") then
    return mock.text("Files in cwd:\n" .. output)
  end
  if tool.name == "process.exec" then
    return mock.text("Working directory: " .. (string.match(output, "^%s*(.-)%s*$") or output))
  end
  return mock.text(output)
end

local function recall_secret(messages, before)
  for i = before - 1, 1, -1 do
    local m = messages[i]
    if m.role == "user" and type(m.content) == "string" then
      local start = string.find(string.lower(m.content), "secret key is ", 1, true)
      if start then
        local value = string.sub(m.content, start + #"secret key is ")
        return string.match(value, "^(.-)%s*[%?%.]*%s*$") or value
      end
    end
  end
end

local function respond(request)
  local messages = request.messages
  local last = messages[#messages]
  local agent = demo_agent(request)
  if agent then return agent end

  local user, user_index = latest_user(messages)
  if last and last.role == "tool" then return after_tool(last, user) end
  if user == nil then return mock.text("[mock provider: no user message]") end

  -- The MAG run's result returns to the lead as a later user message.
  if contains(user, COMBINED_TEXT) then
    return mock.text(COMBINED_TEXT, { reasoning = RELAY_REASONING })
  end

  local low = string.lower(user)
  if string.find(low, "octopus", 1, true) and string.find(low, "lighthouse", 1, true) then
    return mock.tool_call("mag-write-file", { file = MAG_FILE, new_string = MAG_PROGRAM },
      { delay_ms = delay(1500) })
  end
  if contains(low, "read readme") then
    return mock.tool_call("read_file", { path = "README.md" })
  end
  if contains(low, "pwd") or contains(low, "where am i") or contains(low, "cwd") then
    return mock.tool_call("process.exec",
      { argv = { "pwd" }, cwd = ".", timeout = { present = true, milliseconds = 5000 } })
  end
  if contains(low, "list files") then
    return mock.tool_call("process.exec",
      { argv = { "ls", "-la" }, cwd = ".", timeout = { present = true, milliseconds = 5000 } })
  end
  if low == "fail" or contains(low, "trigger error") then
    return mock.error("Mock provider triggered error on user request.")
  end
  if contains(low, "what is the secret key") then
    local value = recall_secret(messages, user_index)
    if value and value ~= "" then return mock.text("The secret key is: " .. value) end
    return mock.text("I don't know yet -- tell me with: \"the secret key is <value>\"")
  end
  if contains(low, "secret key is ") then
    return mock.text("Got it. The secret key is now in this chat's history.")
  end
  if contains(low, "translate hello") then
    local greeting = contains(low, "chinese") and "你好" or "こんにちは"
    return mock.text("**Hello** in that language is `" .. greeting .. "`.")
  end
  local count = string.match(low, "count to (%d+)")
  if count then
    local n = math.max(1, math.min(50, tonumber(count) or 1))
    local parts = {}
    for i = 1, n do parts[i] = tostring(i) end
    return mock.text(table.concat(parts, ", "))
  end
  local topic = string.match(low, "think out loud about (.+)") or string.match(low, "reason about (.+)")
  if topic then
    topic = string.match(topic, "^(.-)%s*[%?%.!]*%s*$") or topic
    return mock.text("Reasoned about " .. topic
      .. "; conclusion: deterministic mock has no opinion, but the reasoning channel works.",
      { reasoning = THINK_REASONING })
  end

  return mock.text(BANNER .. "\n\n[mock unmatched input: " .. user .. "]\n" .. HELP_BODY)
end

mock.serve {
  respond = respond,
  -- ~200 tok/s: 16-codepoint chunks every 20ms read as live output.
  pace_ms = fast and 0 or 20,
}
