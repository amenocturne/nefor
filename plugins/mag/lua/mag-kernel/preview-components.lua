-- Ordinary constructor functions for shipped factory previews.
local preview = require("preview")
local M = {}

local function title(text)
  return preview.text { value = text, style = "section", wrap = "word" }
end

local function section(label, child)
  return preview.column { gap = 0, children = { title(label), child } }
end

local function value_view(label, binding)
  return section(label, preview.value { value = binding, format = "typed", wrap = "word" })
end

local function input_output(declaration)
  return preview.column { gap = 1, children = {
    value_view("Input", preview.input("last")),
    value_view("Output", preview.output("last")),
    preview.text { value = preview.lifecycle("status"), style = "status", wrap = "none" },
  } }
end

local TRANSCRIPT_SCHEMA = { kind = "variant", tag = "kind", cases = {
  reasoning = { kind = "record", fields = { text = "string" } },
  assistant = { kind = "record", fields = { text = "string" } },
  tool_call = { kind = "record", fields = { value = "data" } },
  tool_result = { kind = "record", fields = { value = "data" } },
  validation = { kind = "record", fields = { value = "data" } },
} }
local function transcript()
  local events = preview.stream_ref("transcript", TRANSCRIPT_SCHEMA)
  return preview.stream {
    source = events,
    follow = "end",
    empty = preview.text { value = "No transcript yet.", style = "dim", wrap = "word" },
    item = preview.cases {
      reasoning = preview.markdown { value = preview.item("text"), theme = "reasoning", wrap = "word" },
      assistant = preview.markdown { value = preview.item("text"), theme = "assistant", wrap = "word" },
      tool_call = preview.value { value = preview.item("value"), format = "typed", style = "tool_call", wrap = "word" },
      tool_result = preview.value { value = preview.item("value"), format = "typed", style = "tool_result", wrap = "word" },
      validation = preview.value { value = preview.item("value"), format = "typed", style = "error", wrap = "word" },
    },
  }
end

local TERMINAL_EVENT_SCHEMA = { kind = "variant", tag = "kind", cases = {
  stdin = { kind = "record", fields = { text = "string" } },
  stdout = { kind = "record", fields = { text = "string" } },
  stderr = { kind = "record", fields = { text = "string" } },
} }
local function terminal(declaration)
  local events = preview.stream_ref("terminal_events", TERMINAL_EVENT_SCHEMA)
  local exit = preview.state("exit", "table?")
  return preview.column { gap = 1, children = {
    section("Command", preview.row { gap = 0, children = {
      preview.text { value = "$ ", style = "command", wrap = "none" },
      preview.text { value = preview.param("command"), style = "command", wrap = "word" },
    } }),
    preview.stream {
      source = events,
      follow = "end",
      empty = preview.text { value = "Waiting for output.", style = "dim", wrap = "word" },
      item = preview.cases {
        stdin = preview.text { value = preview.item("text"), style = "stdin", wrap = "word" },
        stdout = preview.text { value = preview.item("text"), style = "stdout", wrap = "word" },
        stderr = preview.text { value = preview.item("text"), style = "stderr", wrap = "word" },
      },
    },
    value_view("Exit", exit),
  } }
end

local function tool_exchange()
  local events = preview.stream_ref("tool_events", { kind = "variant", tag = "kind", cases = {
    call = { kind = "record", fields = { value = "data" } },
    result = { kind = "record", fields = { value = "data" } },
    error = { kind = "record", fields = { value = "data" } },
  } })
  return preview.stream {
    source = events, follow = "end",
    empty = preview.text { value = "No tool activity yet.", style = "dim", wrap = "word" },
    item = preview.cases {
      call = preview.value { value = preview.item("value"), format = "typed", style = "tool_call", wrap = "word" },
      result = preview.value { value = preview.item("value"), format = "typed", style = "tool_result", wrap = "word" },
      error = preview.value { value = preview.item("value"), format = "typed", style = "error", wrap = "word" },
    },
  }
end

local function structured_output()
  return preview.column { gap = 1, children = {
    transcript(), value_view("Final value", preview.output("last")),
  } }
end

local function human()
  return preview.column { gap = 1, children = {
    value_view("Prompt", preview.param("prompt")), value_view("Subject", preview.input("last")),
    value_view("Decision", preview.output("last")),
  } }
end

local function source() return value_view("Initial value", preview.param("value")) end
local function output() return value_view("Result boundary", preview.output("last")) end
local function sink() return value_view("Consumed value", preview.input("last")) end

M.section = section
M.value_view = value_view
M.input_output = input_output
M.transcript = transcript
M.terminal = terminal
M.tool_exchange = tool_exchange
M.structured_output = structured_output
M.human = human
M.source = source
M.output = output
M.sink = sink
return M
