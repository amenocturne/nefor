local manager = require("libs.conversation-manager")
local projection = require("libs.conversation-manager.projection")
local domain = manager.domain
local store = manager.new()
local json = nefor.json
local serial = 0
local function add(kind, fields)
  serial = serial + 1
  local fact = { event_id = "wire:" .. serial, conversation_id = "wire", kind = kind }
  for key, value in pairs(fields or {}) do fact[key] = value end
  local value, err = store:append(fact)
  assert(value, err and err.code)
  return value
end
local function message(id, role, content, extra)
  local fields = { message_id = id, role = role }
  for key, value in pairs(extra or {}) do fields[key] = value end
  add("message_started", fields)
  add("content_chunk_appended", { message_id = id,
    chunk = { kind = type(content) == "table" and "structured" or "text", data = content } })
  add("message_completed", { message_id = id })
end
local opaque = { provider = "chatgpt", model = "model",
  format = "chatgpt.responses.output_items.v1",
  artifact = { items = {{ type = "reasoning", encrypted_content = "sealed", extra = { unknown = true } }} } }
local function compact(id, cutoff)
  local c = store:peek("wire")
  local binding = { request_id = id, provider = "chatgpt", model = "model",
    epoch_id = domain.policy_state(c).epoch_id, history_id = domain.copy(c.head),
    context_watermark = c.last_sequence, trigger = "manual", history_cutoff = cutoff }
  add("context_compaction_requested", binding)
  binding.checkpoint = { provider = "chatgpt", model = "model", format = "chatgpt.responses.compaction.v1",
    artifact = { items = {{ type = "compaction", encrypted_content = "checkpoint", extra = { unknown = true } }} } }
  binding.compatibility = { provider = "chatgpt", model = "model", format = binding.checkpoint.format }
  add("context_compaction_completed", binding)
end
add("created", { provenance = { provider = "chatgpt", model = "model", purpose = "lead" } })
message("system", "system", "instructions")
-- A prefix checkpoint leaves the large tool result in the tail, rather than
-- making the regression pass by compacting away its media.
compact("prefix", 1)
add("message_started", { message_id = "call", role = "assistant" })
add("tool_exchange_started", { exchange_id = "exchange", message_id = "call", tool_name = "read_image" })
add("tool_call_completed", { exchange_id = "exchange",
  call = { id = "image-call", name = "read_image", arguments = { path = "synthetic.png" } } })
local image = { type = "media", media_type = "image/png", filename = "synthetic.png",
  data = string.rep("A", 2 * 1024 * 1024) }
add("tool_result_recorded", { exchange_id = "exchange", result = image })
add("message_completed", { message_id = "call", provider_context = opaque })
message("image", "tool", image, { tool_call_id = "image-call", tool_name = "read_image" })
message("question", "user", "continue from this image")
local question = domain.copy(store:peek("wire").message_by_id.question.history_id)

local function check(context, adapter, compatible)
  local before = json.encode(context)
  local expected = compatible and context.tail_messages or context.messages
  local t = adapter == "chatgpt" and require("chatgpt-provider").translator(adapter)
    or provider.translator(adapter)
  local body = t.complete({ request_id = "request", provider = adapter, model = "model",
    conversation_id = "wire", tools = { "read_image" } }, context)
  t.deliver(body)
  local row = _delivered_log[#_delivered_log]
  assert(row.wire_bytes < 16 * 1024 * 1024)
  -- The complete/deliver path must carry precisely one selected history. These
  -- are the fields consumed by the Rust dispatcher, not transcript metadata.
  local actual
  if compatible then
    assert(row.body.messages == nil)
    assert(row.body.conversation_context.messages == nil)
    assert(domain.equal(row.body.conversation_context.compaction.checkpoint, context.compaction.checkpoint))
    actual = row.body.conversation_context.tail_messages
  else
    assert(row.body.conversation_context == nil)
    actual = row.body.messages
  end
  assert(#actual == #expected)
  local saw_image, saw_opaque = false, false
  for index, source in ipairs(expected) do
    local wire = actual[index]
    assert(wire.role == source.role and domain.equal(wire.content, source.content))
    assert(wire.tool_call_id == source.tool_call_id and wire.tool_name == source.tool_name)
    assert(domain.equal(wire.provider_context, source.provider_context))
    assert(wire.chunks == nil and wire.structured == nil and wire.text == nil and wire.history_id == nil)
    if source.role == "tool" then
      assert(wire.content.data == image.data and wire.content.media_type == image.media_type)
      saw_image = true
    elseif source.id == "call" then
      local call = wire.tool_calls[1]
      assert(call.id == "image-call" and call["function"].name == "read_image")
      assert(domain.equal(json.decode(call["function"].arguments), { path = "synthetic.png" }))
      assert(domain.equal(wire.provider_context, opaque))
      saw_opaque = true
    end
  end
  assert(saw_image and saw_opaque)
  if adapter == "chatgpt" then
    local plan = assert(t.compact_context({ request = { request_id = "compact", conversation_id = "wire",
      provider = adapter, model = "model", trigger = "manual" }, context = context }))
    assert(domain.equal(plan.messages, actual), "compaction and completion consume the same model messages")
    if compatible then
      assert(domain.equal(plan.restore.model_context_artifact, context.compaction.checkpoint.artifact))
    else
      assert(plan.restore == nil)
    end
  end
  print(adapter .. (compatible and " checkpoint" or " full") .. " frame bytes: " .. row.wire_bytes)
  assert(json.encode(context) == before, "lowering must not mutate canonical projections")
end

local context = projection.context(store:peek("wire"))
-- This fixture exceeds the real frame bound under the previous three-history
-- layout, including the media repeated in content/chunks/structured.
local redundant_bytes = #json.encode({ type = "event", from = "engine", ts = 0,
  body = { messages = context.messages, conversation_context = context } }) + 1
assert(redundant_bytes > 16 * 1024 * 1024, "fixture must reproduce oversized delivery")
print("redundant provider frame bytes: " .. redundant_bytes)
check(context, "chatgpt", true)
check(context, "openai", false)
local no_checkpoint = domain.copy(context)
no_checkpoint.compaction = nil
check(no_checkpoint, "chatgpt", false)
for _, field in ipairs({ "provider", "model", "format" }) do
  local incompatible = domain.copy(context)
  incompatible.compaction.checkpoint[field] = "incompatible"
  check(incompatible, "chatgpt", false)
end
-- Rewind drops a newer checkpoint and the latest prompt, but retains the
-- earlier image and compatible prefix checkpoint. Continue must still fit.
compact("later", 4)
add("rewind_committed", { target_history_id = question, expected_head = domain.copy(store:peek("wire").head) })
local rewound = projection.context(store:peek("wire"))
assert(rewound.compaction.request_id == "prefix")
assert(#rewound.messages == 3)
check(rewound, "chatgpt", true)
check(rewound, "openai", false)
-- Rewind is append-only: the original structured media remains canonical.
assert(store:peek("wire").message_by_id.image.chunks[1].data.data == image.data)
