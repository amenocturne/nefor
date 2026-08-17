-- tests/lua/mag-kernel/adapter_test.lua — unit tests for the entry adapter
-- factory (plugins/mag/lua/mag-kernel/factories/adapter.lua).
--
-- Driven from engine/tests/starter_mag_kernel_test.rs (installs the minimal
-- nefor.log surface, points package.path at plugins/mag/lua/mag-kernel/). Tests the
-- factory in isolation: a capturing `emit` stands in for the kernel outbound.
-- The adapter is the agent's boundary type shift — it lifts either the initial
-- task seed OR an upstream agent's TextAnswer into the `ProviderInput` turn the
-- downstream `llm` consumes. Both directions are asserted here.

local Registry = require("registry")
local adapter  = require("factories.adapter")
local llm      = require("factories.llm")
local schema = { version = 1, root = {
  kind = "named", name = "Task", body = { kind = "string" }
} }

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

-- Capturing emit sink standing in for the kernel's outbound.
local function capture()
  local out = {}
  return out, function(message) out[#out + 1] = message end
end

local function find_kind(msgs, kind)
  for _, m in ipairs(msgs) do
    if m.kind == kind then return m end
  end
  return nil
end

local function conversation_messages(facts)
  local messages, by_id = {}, {}
  for _, fact in ipairs(facts) do
    if fact.kind == "message_started" then
      local message = {
        role = fact.role,
        text = "",
        structured = {},
        completed = false,
      }
      messages[#messages + 1] = message
      by_id[fact.message_id] = message
    elseif fact.kind == "content_chunk_appended" then
      local message = by_id[fact.message_id]
      if message and fact.chunk.kind == "text" then
        message.text = message.text .. fact.chunk.data
      elseif message and fact.chunk.kind == "structured" then
        message.structured[#message.structured + 1] = fact.chunk.data
      end
    elseif fact.kind == "message_completed" then
      local message = by_id[fact.message_id]
      if message then message.completed = true end
    end
  end
  local completed = {}
  for _, message in ipairs(messages) do
    if message.completed then
      if message.text ~= "" then
        message.content = message.text
      elseif #message.structured == 1 then
        message.content = message.structured[1]
      else
        message.content = message.structured
      end
      completed[#completed + 1] = message
    end
  end
  return completed
end

-- Build a single boundary activation (routing.lua, the kernel⇄factory
-- contract): one delivered { from, tag, message } triple. The adapter is a
-- union-input actor firing on any, so one triple is the activation it sees.
local function single(from, tag, message)
  return { shape = "union", messages = { { from = from, tag = tag, message = message } } }
end

-- ==================================================================
-- declaration: union input, ProviderInput output, no signals
-- ==================================================================

do
  local reg = Registry.new()
  local decl, err = reg:register({ declaration = adapter.declaration, construct = adapter.construct })
  assert_true(decl ~= nil and err == nil, "adapter factory registers cleanly: " .. tostring(err))

  -- Union input mentions both boundary tags (fires on either).
  local input = reg:declared_input("adapter", "boundary")
  assert_true(type(input) == "table", "adapter declares a union boundary input")
  local tags = {}
  for _, t in ipairs(input) do tags[t] = true end
  assert_true(tags["task"], "boundary input accepts the initial task seed")
  assert_true(tags["generic-provider.TextAnswer"], "boundary input accepts an upstream TextAnswer")

  assert_eq(decl.outputs[1], "generic-provider.ProviderOut", "adapter output is the provider turn")
  assert_eq(#decl.signals, 0, "adapter is synchronous — declares no signal handlers")
end

-- ==================================================================
-- ready on construct, signs with id
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = adapter.construct("docs-explorer.entry", { seed = "provider-in", schema = schema }, emit)
  assert_true(inst ~= nil, "adapter constructs")
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil, "adapter emits ready")
  assert_eq(ready.from, "docs-explorer.entry", "ready is id-signed")
end

-- ==================================================================
-- task seed in -> ProviderInput out (source agent's initial activation)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = adapter.construct("docs-explorer.entry", { seed = "provider-in", schema = schema }, emit)

  local completion = inst.deliver(single("__initial",
    "task", { kind = "task", prompt = "explore the codebase" }))
  assert_eq(completion.status, "ok", "synchronous shift returns a successful completion")

  local out = find_kind(msgs, "generic-provider.ProviderOut")
  assert_true(out ~= nil, "the task seed lifts into a ProviderInput turn")
  assert_eq(out.from, "docs-explorer.entry", "ProviderInput is id-signed")
  assert_eq(#out.messages, 1, "one turn message for the downstream provider")
  assert_eq(out.messages[1].role, "user", "the seed becomes a user-role turn")
  assert_eq(out.messages[1].content.mag_type.root.name, "Task",
    "the declared semantic identity accompanies the turn")
  assert_eq(out.messages[1].content.value.prompt, "explore the codebase",
    "the complete typed task is the turn value")
end

-- ==================================================================
-- typed task projection boundaries preserve envelopes or yield bounded text
-- ==================================================================

do
  local task_schema = { version = 1, root = {
    kind = "named", name = "nefor.contracts.Task", body = {
      kind = "record", fields = { { name = "prompt", schema = { kind = "string" } } },
    },
  } }
  local function deliver_at_size(target_size)
    local value = { prompt = "" }
    local envelope = { mag_type = task_schema, value = value }
    local overhead = #nefor.json.encode(envelope)
    value.prompt = string.rep("x", target_size - overhead)
    assert_eq(#nefor.json.encode(envelope), target_size, "fixture reaches the exact projected size")
    local canonical = { value = value, output_path = "/runs/task/output.json" }
    local msgs, emit = capture()
    local inst = adapter.construct("lead.entry", { schema = task_schema }, emit)
    inst.deliver(single("lead.source", "task", canonical))
    return find_kind(msgs, "generic-provider.ProviderOut"), canonical
  end

  for _, size in ipairs({ 32767, 32768 }) do
    local out, canonical = deliver_at_size(size)
    assert_true(type(out.value.content) == "table",
      "a typed Task at or below 32768 bytes remains a typed provider value")
    assert_eq(out.value.content.prompt, canonical.value.prompt,
      "the preserved typed Task supplies valid ProviderInput content")
  end

  local above, canonical = deliver_at_size(32769)
  assert_true(type(above.value.content) == "string",
    "an oversized typed Task supplies bounded text as ProviderInput content")
  assert_true(#above.value.content <= 32768, "oversized Task projection obeys the 32 KiB cap")
  assert_true(above.value.content:find("output truncated", 1, true) ~= nil,
    "oversized Task projection carries a truncation marker")
  assert_eq(#canonical.value.prompt, 32769 - (#nefor.json.encode({
    mag_type = task_schema, value = { prompt = "" },
  })), "the canonical upstream Task remains unchanged")
end

-- ==================================================================
-- TextAnswer in -> ProviderInput out (upstream agent hand-off)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = adapter.construct("code-writer.entry", { seed = "provider-in", schema = schema }, emit)

  -- text_answer preferred when present.
  inst.deliver(single("docs-explorer.llm", "generic-provider.TextAnswer",
    { kind = "generic-provider.TextAnswer", text_answer = "found the bug in foo.rs", text = "raw text" }))
  local out = find_kind(msgs, "generic-provider.ProviderOut")
  assert_true(out ~= nil, "an upstream TextAnswer lifts into a ProviderInput turn")
  assert_eq(out.from, "code-writer.entry", "ProviderInput is id-signed")
  assert_eq(out.messages[1].role, "user", "the hand-off becomes a user-role turn")
  assert_eq(out.messages[1].content.value, "found the bug in foo.rs",
    "text_answer is preferred as the turn content")
end

-- ==================================================================
-- TextAnswer content fallbacks: text, then raw result
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = adapter.construct("code-writer.entry", { schema = schema }, emit)

  -- text used when no text_answer.
  inst.deliver(single("up", "generic-provider.TextAnswer",
    { kind = "generic-provider.TextAnswer", text = "just the text" }))
  local out1 = msgs[#msgs]
  assert_eq(out1.messages[1].content.value, "just the text", "text is used when text_answer is absent")

  -- raw result passes through verbatim when neither text_answer nor text.
  inst.deliver(single("up", "generic-provider.TextAnswer",
    { kind = "generic-provider.TextAnswer", result = { nested = "structured" } }))
  local out2 = msgs[#msgs]
  assert_eq(out2.messages[1].content.value.nested, "structured",
    "the raw result passes through verbatim for the provider layer to serialize")
end

-- ==================================================================
-- separately routed product -> ordered provider-native message list
-- ==================================================================

do
  local task_schema = { kind = "named", name = "Task", body = { kind = "string" } }
  local answer_schema = { kind = "union", variants = {
    { tag = "final-id", schema = { kind = "named", name = "TextAnswer", body = { kind = "string" } } },
    { tag = "error-id", schema = { kind = "named", name = "AgentError", body = { kind = "string" } } },
  } }
  local product_schema = { version = 1, root = { kind = "product", components = {
    task_schema, answer_schema, answer_schema,
  } } }
  local msgs, emit = capture()
  local inst = adapter.construct("fan-in.entry", { schema = product_schema }, emit)
  inst.deliver({ shape = "product", messages = {
    { tag = "task", message = { value = { prompt = "coordinate" } },
      arrival = { constructor_id = "task-id" } },
    { tag = "generic-provider.TextAnswer", message = { text_answer = "done" },
      arrival = { constructor_id = "final-id" } },
    { tag = "nefor.agent.Result", message = { value = { error = "blocked" } },
      arrival = { constructor_id = "error-id" } },
  } })

  local out = find_kind(msgs, "generic-provider.ProviderOut")
  assert_eq(#out.messages, 3, "every product component becomes one provider message")
  assert_eq(out.messages[1].content.mag_type.root.name, "Task",
    "position one carries the Task schema")
  assert_eq(out.messages[1].content.value.prompt, "coordinate",
    "position one includes the complete Task value")
  assert_eq(out.messages[2].content.mag_type.root.kind, "union",
    "position two carries its own union schema")
  assert_eq(out.messages[2].content.value.type, "final-id",
    "position two preserves the selected TextAnswer constructor")
  assert_eq(out.messages[2].content.value.value, "done",
    "position two includes the TextAnswer payload")
  assert_eq(out.messages[3].content.value.type, "error-id",
    "position three preserves the selected AgentError constructor")
  assert_eq(out.messages[3].content.value.value.error, "blocked",
    "position three includes the complete AgentError payload")
end

-- ==================================================================
-- whole product on one edge remains one whole-product message
-- ==================================================================

do
  local product_schema = { version = 1, root = { kind = "product", components = {
    { kind = "named", name = "Task", body = { kind = "string" } },
    { kind = "named", name = "TextAnswer", body = { kind = "string" } },
  } } }
  local msgs, emit = capture()
  local inst = adapter.construct("whole.entry", { schema = product_schema }, emit)
  inst.deliver({ shape = "product", whole = true, messages = {
    { tag = "task", message = { value = { { prompt = "go" }, "done" } } },
  } })
  local out = find_kind(msgs, "generic-provider.ProviderOut")
  assert_eq(#out.messages, 1, "a whole product remains one provider message")
  assert_eq(out.messages[1].content.mag_type.root.kind, "product",
    "the whole-product message carries the whole schema")
  assert_eq(out.messages[1].content.value[2], "done", "the whole tuple remains intact")
end

-- ==================================================================
-- provider-free adapter -> llm boundary integration
-- ==================================================================

do
  local product_schema = { version = 1, root = { kind = "product", components = {
    { kind = "named", name = "Task", body = { kind = "string" } },
    { kind = "union", variants = {
      { tag = "final-id", schema = { kind = "named", name = "TextAnswer", body = { kind = "string" } } },
      { tag = "error-id", schema = { kind = "named", name = "AgentError", body = { kind = "string" } } },
    } },
  } } }
  local provider_messages, provider_emit = capture()
  local facts = {}
  local provider = assert(llm.construct("fan-in.llm", {
    provider = "fake-provider",
    output_type = "final-id",
    error_type = "error-id",
    provider_error_type = "provider-error-id",
  }, provider_emit, {
    conversation = {
      id = "fan-in:conversation",
      turn_id = "fan-in:turn",
      emit = function(fact) facts[#facts + 1] = fact end,
    },
  }))
  local adapter_messages, adapter_emit = capture()
  local entry = adapter.construct("fan-in.entry", { schema = product_schema }, function(message)
    adapter_emit(message)
    if message.kind == "generic-provider.ProviderOut" then
      provider.deliver(single("fan-in.entry", message.kind, message))
    end
  end)
  entry.deliver({ shape = "product", messages = {
    { tag = "task", message = { value = { prompt = "go" } } },
    { tag = "generic-provider.TextAnswer", message = {
        value = { content = string.rep("v", 2048) },
        transcript_delta = { { role = "tool", content = string.rep("x", 2 * 1024 * 1024) } },
        result = { raw_log = string.rep("y", 2 * 1024 * 1024) },
      },
      arrival = { constructor_id = "final-id" } },
  } })

  local invoke = find_kind(provider_messages, "capability.invoke")
  assert_true(invoke ~= nil, "the provider boundary emits a provider-free capability.invoke")
  assert_true(invoke.request.input == nil,
    "capability.invoke does not duplicate the adapter's native message list")
  local history = conversation_messages(facts)
  assert_eq(#history, 2, "the adapter's native message list is recorded once")
  assert_eq(history[1].content.mag_type.root.name, "Task",
    "canonical position one retains the Task envelope")
  assert_eq(history[2].content.value.type, "final-id",
    "canonical position two retains constructor identity")
  assert_eq(#history[2].content.value.value.content, 2048,
    "canonical context receives the final value")
  assert_eq(history[2].content.value.value.transcript_delta, nil,
    "canonical context receives no worker transcript metadata")
  assert_eq(history[2].content.value.value.result, nil,
    "canonical context receives no raw provider result")
end

print("mag-kernel adapter_test: all assertions passed")
