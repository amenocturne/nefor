local service_lib = require("libs.conversation-manager.service")
local domain = require("libs.conversation-manager").domain

local function eq(actual, expected, message)
  assert(domain.equal(actual, expected), (message or "values differ")
    .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
end

local service = service_lib.new()
local writer = service:writer()
local reader = service:reader()

assert(writer ~= reader, "writer and reader are separate capability facets")
eq(reader.append, nil, "reader exposes no append authority")
eq(reader.apply_recorded, nil, "reader exposes no replay authority")
eq(reader.reset, nil, "reader exposes no reset authority")

local conversation, err = writer:append({
  event_id = "created",
  conversation_id = "shared",
  kind = "created",
  provenance = { surface = "test" },
})
assert(conversation, err and err.code)
conversation, err = writer:append({
  event_id = "system-start",
  conversation_id = "shared",
  kind = "message_started",
  message_id = "system",
  role = "system",
})
assert(conversation, err and err.code)
conversation, err = writer:append({
  event_id = "system-text",
  conversation_id = "shared",
  kind = "content_chunk_appended",
  message_id = "system",
  chunk = { kind = "text", data = "canonical system" },
})
assert(conversation, err and err.code)
conversation, err = writer:append({
  event_id = "system-done",
  conversation_id = "shared",
  kind = "message_completed",
  message_id = "system",
})
assert(conversation, err and err.code)

eq(reader:watermark("shared"), 4, "reader exposes the manager watermark")
local context = reader:context("shared")
eq(context.history_length, 1)
eq(context.messages[1].role, "system")
eq(context.messages[1].text, "canonical system")

context.messages[1].chunks[1].data = "mutated"
context.messages[1].text = "mutated"
local fresh = reader:context("shared")
eq(fresh.messages[1].text, "canonical system", "context reads are immutable copies")
eq(fresh.messages[1].chunks[1].data, "canonical system")

local before = writer:stats()
for _ = 1, 50 do reader:context("shared") end
eq(writer:stats(), before, "reads retain no additional manager state")

writer:reset()
eq(reader:conversation("shared"), nil, "existing reader follows reset backing state")
eq(reader:watermark("shared"), nil)
eq(#reader:list(), 0)

conversation, err = writer:append({
  event_id = "after-reset",
  conversation_id = "new",
  kind = "created",
})
assert(conversation, err and err.code)
eq(reader:watermark("new"), 1, "existing reader follows replacement state")

print("conversation_manager_service_test: all assertions passed")
