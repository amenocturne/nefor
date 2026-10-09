-- Provider-shaped mock for the `mock-plugin` script host.
--
-- Speaks the same direct-completion wire shape as openai-provider and leaves
-- every answer to the behavior its configuration supplies:
--
--   <name>.completion.request { request_id, request = { system?, input = { messages }, tools?, ... } }
--   <name>.completion.cancel  { request_id }
--   -> <name>.completion.event { request_id, event = "reasoning_delta" | "reasoning_end"
--                                | "text_delta" | "usage" | "completed" | "error", ... }
--
-- plus the lifecycle the starter's provider adapter expects: `hello` and a
-- permanent `auth.status = connected` after ready_ok, `auth.set`,
-- `models.list_requested`, and `model.set`.
--
-- Usage (inside a script loaded by `mock-plugin --script ... --lua-root <lua>`):
--
--   local mock = require("libs.mock-provider")
--   mock.serve {
--     respond = function(request)
--       return mock.text("hello")
--     end,
--   }
--
-- `respond(request)` receives `{ id, model, system, messages, tools, body }`.
-- `system` joins the request's `system` field and every system-role message
-- (nil when there is none). Each message keeps its wire fields: `role`, `content` (string or decoded
-- table, unchanged), `name` (tool name for tool results), `tool_call_id`,
-- `tool_calls`. It returns a response built with the helpers below. A behavior
-- that raises or returns something else ends the request with a completion
-- error naming the fault, so a broken scenario fails visibly instead of hanging.

local M = {}

local DEFAULT_MODEL = "mock-model"
local DEFAULT_DURATION_MS = 250
local DEFAULT_CHUNK_CODEPOINTS = 16
local DELAY_SLICE_MS = 100

local tool_id_counter = 0

-- Fresh ids per call: consumers correlate tool results by exact id, so a
-- repeated id across turns would attach a later result to an earlier call.
local function mint_tool_id(name)
  tool_id_counter = tool_id_counter + 1
  return "call_mock_" .. tostring(name):gsub("[^%w_]", "_") .. "_" .. tostring(tool_id_counter)
end

local function with_options(response, opts)
  opts = opts or {}
  response.reasoning = opts.reasoning
  response.delay_ms = opts.delay_ms
  return response
end

--- A streamed text answer. `opts.reasoning` (string or list of chunks) is
--- streamed on the reasoning channel first; `opts.delay_ms` waits before the
--- first event.
function M.text(text, opts)
  assert(type(text) == "string", "mock.text: text must be a string")
  return with_options({ kind = "text", text = text }, opts)
end

--- One tool call: `mock.tool_call("read_file", { path = "README.md" })`.
function M.tool_call(name, arguments, opts)
  return M.tool_calls({ { name = name, arguments = arguments } }, opts)
end

--- Several tool calls in one response: `{ { name, arguments }, ... }`.
--- Text may accompany them through `opts.text`.
function M.tool_calls(calls, opts)
  assert(type(calls) == "table" and #calls > 0, "mock.tool_calls: at least one call is required")
  local out = {}
  for i, call in ipairs(calls) do
    assert(type(call.name) == "string" and call.name ~= "",
      "mock.tool_calls: call " .. i .. " needs a name")
    out[i] = { id = call.id or mint_tool_id(call.name), name = call.name, arguments = call.arguments or {} }
  end
  local response = with_options({ kind = "tool_calls", tool_calls = out }, opts)
  response.text = opts and opts.text or ""
  return response
end

--- A terminal provider error for this request.
function M.error(message, opts)
  assert(type(message) == "string", "mock.error: message must be a string")
  return with_options({ kind = "error", message = message }, opts)
end

local function normalize_request(body)
  local request = type(body.request) == "table" and body.request or body
  local input = type(request.input) == "table" and request.input or {}
  local raw_messages = type(input.messages) == "table" and input.messages or request.messages
  local messages = {}
  for _, message in ipairs(type(raw_messages) == "table" and raw_messages or {}) do
    messages[#messages + 1] = {
      role = message.role,
      content = message.content,
      name = message.name or message.tool_name,
      tool_call_id = message.tool_call_id,
      tool_calls = message.tool_calls,
    }
  end
  -- The system instruction may ride the request's `system` field or
  -- system-role messages; behaviors see both joined in wire order.
  local system_parts = {}
  if type(request.system) == "string" and request.system ~= "" then
    system_parts[1] = request.system
  end
  for _, message in ipairs(messages) do
    if message.role == "system" and type(message.content) == "string" then
      system_parts[#system_parts + 1] = message.content
    end
  end
  local system = #system_parts > 0 and table.concat(system_parts, "\n\n") or nil
  return {
    id = body.request_id,
    model = request.model,
    system = system,
    messages = messages,
    tools = type(request.tools) == "table" and request.tools or {},
    body = body,
  }
end

local function validate_response(response)
  if type(response) ~= "table" then
    return nil, "behavior returned " .. type(response) .. " instead of a response"
  end
  if response.kind == "text" or response.kind == "tool_calls" or response.kind == "error" then
    return response
  end
  return nil, "behavior returned a table that is not a mock.text/tool_call(s)/error response"
end

local function reasoning_chunks(reasoning)
  if reasoning == nil then return nil end
  if type(reasoning) == "string" then return { reasoning } end
  assert(type(reasoning) == "table", "mock response reasoning must be a string or a list of strings")
  return reasoning
end

local function codepoint_chunks(text, size)
  local chunks = {}
  local count = utf8.len(text) or #text
  local first = 1
  -- Chunk boundaries snap to codepoints: slicing inside a multibyte sequence
  -- produces invalid UTF-8 that the JSON wire rejects.
  while first <= count do
    local last = math.min(first + size - 1, count)
    local byte_start = utf8.offset(text, first)
    local byte_after = utf8.offset(text, last + 1)
    chunks[#chunks + 1] = string.sub(text, byte_start, byte_after and (byte_after - 1) or #text)
    first = last + 1
  end
  return chunks
end

--- Register the provider handlers on the host's `nefor` table.
---
--- config:
---   respond(request) -> response   required
---   model           string         default "mock-model"
---   models          { string }     answered to models.list_requested; default { model }
---   duration_ms     number         reported on usage/completed; default 250
---   chunk_codepoints number        text/reasoning chunk size; default 16
---   pace_ms         number         pause between chunks; default 0 (no pause)
---   hello           false | { delay_ms = n }  withhold or delay the ready hello
function M.serve(config)
  assert(type(config) == "table" and type(config.respond) == "function",
    "mock-provider.serve: config.respond(request) is required")
  local name = nefor.name
  local model = config.model or DEFAULT_MODEL
  local models = config.models or { model }
  local duration_ms = config.duration_ms or DEFAULT_DURATION_MS
  local chunk_codepoints = config.chunk_codepoints or DEFAULT_CHUNK_CODEPOINTS
  local pace_ms = config.pace_ms or 0
  local hello = config.hello == nil and {} or config.hello

  local in_flight = {}

  local function emit_event(request_id, event, fields)
    local body = fields or {}
    body.request_id = request_id
    body.event = event
    nefor.emit("completion.event", body)
  end

  -- `nefor.sleep` yields to the host, so a cancel envelope can land between
  -- chunks; each wait returns false once the request has been cancelled.
  local function wait(run, ms)
    local remaining = ms
    while remaining > 0 do
      if run.cancelled then return false end
      local slice = math.min(remaining, DELAY_SLICE_MS)
      nefor.sleep(slice)
      remaining = remaining - slice
    end
    return not run.cancelled
  end

  local function stream(run, request_id, event, chunks)
    for i, chunk in ipairs(chunks) do
      if run.cancelled then return false end
      emit_event(request_id, event, { text = chunk })
      if i < #chunks and pace_ms > 0 and not wait(run, pace_ms) then return false end
    end
    return not run.cancelled
  end

  local function settle(request_id, run)
    if in_flight[request_id] == run then in_flight[request_id] = nil end
  end

  local function complete(run, request_id, response)
    if type(response.delay_ms) == "number" and response.delay_ms > 0
        and not wait(run, response.delay_ms) then
      return
    end

    if response.kind == "error" then
      emit_event(request_id, "error", { message = response.message })
      return
    end

    local reasoning = reasoning_chunks(response.reasoning)
    local reasoning_text
    if reasoning then
      if not stream(run, request_id, "reasoning_delta", reasoning) then return end
      reasoning_text = table.concat(reasoning)
      emit_event(request_id, "reasoning_end", { text = reasoning_text, duration_ms = duration_ms })
    end

    -- Tool-call turns carry their text only in the result, matching
    -- openai-provider, which streams no deltas for them.
    local text = response.text or ""
    if response.kind == "text" and #text > 0
        and not stream(run, request_id, "text_delta", codepoint_chunks(text, chunk_codepoints)) then
      return
    end
    if run.cancelled then return end

    local completion_tokens = #text
    local result = {
      text = text,
      finish_reason = response.kind == "tool_calls" and "tool_calls" or "stop",
      usage = { prompt_tokens = 0, completion_tokens = completion_tokens, model = model },
      tool_calls = response.tool_calls,
      reasoning = reasoning_text,
    }
    emit_event(request_id, "usage", {
      prompt_tokens = 0,
      completion_tokens = completion_tokens,
      model = model,
      duration_ms = duration_ms,
    })
    emit_event(request_id, "completed", { result = result, model = model, duration_ms = duration_ms })
  end

  nefor.on_ready_ok(function()
    if hello ~= false then
      if type(hello.delay_ms) == "number" and hello.delay_ms > 0 then nefor.sleep(hello.delay_ms) end
      nefor.emit("hello", { model = model })
    end
    -- The mock has no credentials; the adapter needs `connected` before the
    -- model picker lists it.
    nefor.emit("auth.status", { state = "connected" })
  end)

  nefor.on(name .. ".completion.request", function(body)
    local request_id = body.request_id
    if type(request_id) ~= "string" or request_id == "" then return end
    if in_flight[request_id] then
      emit_event(request_id, "error", {
        message = "completion request `" .. request_id .. "` is already in flight",
      })
      return
    end
    local run = { cancelled = false }
    in_flight[request_id] = run

    local ok, response = pcall(config.respond, normalize_request(body))
    local fault
    if ok then
      response, fault = validate_response(response)
    else
      fault = "behavior raised: " .. tostring(response)
    end
    if fault then
      nefor.log("mock-provider: " .. fault)
      emit_event(request_id, "error", { message = "mock-provider: " .. fault })
      settle(request_id, run)
      return
    end

    complete(run, request_id, response)
    settle(request_id, run)
  end)

  nefor.on(name .. ".completion.cancel", function(body)
    local request_id = body and body.request_id
    local run = type(request_id) == "string" and in_flight[request_id] or nil
    if run then
      run.cancelled = true
      -- Cancellation settles the correlation id immediately; the detached
      -- request observes the flag at its next yield and emits nothing more,
      -- so a caller may reuse the id without racing that task.
      in_flight[request_id] = nil
    end
  end)

  nefor.on(name .. ".auth.set", function()
    nefor.emit("auth.status", { state = "connected" })
  end)

  nefor.on(name .. ".models.list_requested", function()
    nefor.emit("models.listed", { models = models })
  end)

  nefor.on(name .. ".model.set", function(body)
    local selected = body and body.model
    if type(selected) ~= "string" or selected == "" then return end
    nefor.emit("model.set_ack", { model = selected })
  end)
end

return M
