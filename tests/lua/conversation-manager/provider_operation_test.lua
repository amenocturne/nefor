-- Engine-side producer contract: a replaceable opaque adapter, not a MAG fake.
local old_engine = nefor.engine
local sent, delivered = {}, {}
nefor.engine = { now = function() return "t" end,
  send = function(payload) sent[#sent + 1] = nefor.json.decode(payload).body end }
local capability_failure, cleanup_failure, lowered_prefix
local translator = {
  kinds = { chat_append = "p.chat.append", chat_error = "p.chat.error", turn_error = "p.turn.error",
    chat_compaction_commit = "p.chat.compaction.commit", completion_event = "p.completion.event",
    models_list_requested = "p.models.list_requested", models_listed = "p.models.listed" },
  compaction_capability = function(_, _, options)
    if capability_failure == "throw" then error("capability refused") end
    if capability_failure == "invalid" then return {} end
    return { kind = options and options.capability or "supported" }
  end,
  compact_context = function(input)
    lowered_prefix = input.context.messages[1].content
    input.context.messages[1].content = "adapter mutation"
    local chat_id = "temporary:" .. input.request.request_id
    return { chat_id = chat_id, create = { kind = "p.chat.create", chat_id = chat_id },
      compact = { kind = "p.chat.compact", chat_id = chat_id }, messages = {} }
  end,
  compaction_outcome = function(body, request)
    return { kind = "completed", request_id = request.request_id, conversation_id = request.conversation_id,
      checkpoint = { provider = "p", model = request.model, format = "alternate.scalar.v1", artifact = "opaque scalar" },
      compatibility = { provider = "p", model = request.model, format = "alternate.scalar.v1" } }
  end,
  checkpoint_compatible = function(cp, provider, model)
    return cp.provider == provider and cp.model == model and cp.format == "alternate.scalar.v1"
  end,
  cleanup_compaction = function(plan)
    if cleanup_failure then error("cleanup refused") end
    return {{ kind = "p.chat.delete", chat_id = plan.chat_id }}
  end,
  cancel_compaction = function(plan) return {{ kind = "p.chat.cancel", chat_id = plan.chat_id }} end,
  deliver = function(body) delivered[#delivered + 1] = body end,
  publish = function(_, body) sent[#sent + 1] = body end,
  maybe_inject_static_token = function() end,
  outbound = function(env) return env.body end,
  inbound = function() return nil end,
  complete = function(request, context) return { request_id = request.request_id, messages = context.messages } end,
}
package.preload["operation-test-adapter"] = function() return { translator = function() return translator end } end
local context = { messages = {}, watermark = 7, history_id = {1} }
local spec = require("libs.compositors.provider").spawn_spec("p", {"unused"}, {
  translator_lib = "operation-test-adapter", agentic_loop = {}, conversations = {
    context = function() return context end } })
local function pending(id)
  return { from = "conversation-manager", body = { kind = "conversation.projection.delta", conversation_id = "c",
    change = { kind = "context_compaction_pending", input = {
      request = { request_id = id, conversation_id = "c", provider = "p", model = "m", epoch_id = "epoch", history_id = {1} },
      context = { messages = {{role = "user", content = "frozen prefix"}} } } } } }
end
local function terminal(id)
  return { from = "p", body = { kind = "p.chat.compaction.commit", request_id = id, chat_id = "temporary:" .. id } }
end
local env = pending("r")
cleanup_failure = true
spec.to_plugin({env, env})
assert(lowered_prefix == "frozen prefix" and env.body.change.input.context.messages[1].content == "frozen prefix")
assert(#delivered == 2, "duplicate admission must not dispatch")
local wrong = terminal("r"); wrong.body.request_id = "wrong"
spec.from_plugin({wrong}); assert(#sent == 0)
spec.from_plugin({terminal("r"), terminal("r")})
assert(#sent == 2 and sent[1].kind == "conversation.context.compact.complete")
assert(sent[1].checkpoint.artifact == "opaque scalar" and sent[1].epoch_id == "epoch" and sent[1].history_id[1] == 1)
assert(sent[2].kind == "conversation.context.compact.cleanup_failed" and sent[2].resource_id == "temporary:r")
spec.to_plugin({env}); assert(#delivered == 2, "settled duplicate is inert")
cleanup_failure = false; sent, delivered = {}, {}
env = pending("cancel"); env.replay = true
spec.to_plugin({env}); assert(#delivered == 0)
env.replay = nil; spec.to_plugin({env}); assert(#delivered == 2)
spec.to_plugin({{from = "consumer", body = {kind = "conversation.context.compact.cancel", request_id = "cancel", conversation_id = "foreign"}}})
assert(#sent == 0 and #delivered == 2)
spec.to_plugin({{from = "consumer", body = {kind = "conversation.context.compact.cancel", request_id = "cancel", conversation_id = "c"}}})
assert(#sent == 1 and sent[1].error.code == "compaction_interrupted")
assert(#delivered == 4 and delivered[3].kind == "p.chat.cancel" and delivered[4].kind == "p.chat.delete")
spec.from_plugin({terminal("cancel")}); assert(#sent == 1, "late native success cannot resurrect cancelled work")
-- Capability exceptions/malformed contracts settle accepted operations, not just preparation.
for _, failure in ipairs({"throw", "invalid"}) do
  capability_failure = failure
  local before = #sent
  spec.to_plugin({pending("bad:" .. failure)})
  assert(#sent == before + 1 and sent[#sent].kind == "conversation.context.compact.failed")
  assert(sent[#sent].error.code == (failure == "throw" and "compaction_capability_failed" or "compaction_capability_invalid"))
  spec.to_plugin({{from = "mag", body = {kind = "conversation.compaction.prepare.request", request_id = "prep:" .. failure,
    provider = "p", model = "m", policy = {kind = "Disabled"}}}})
  assert(sent[#sent].request_id == "prep:" .. failure and sent[#sent].outcome.kind == "Error")
end
capability_failure = nil; sent, delivered = {}, {}
local function prepare(id, policy, options)
  spec.to_plugin({{from = "mag", body = {kind = "conversation.compaction.prepare.request", request_id = id,
    provider = "p", model = "m", policy = policy, provider_options = options}}})
end
prepare("disabled", {kind = "Disabled"}, {capability = "unknown"})
assert(sent[1].outcome.kind == "Ready" and sent[1].outcome.capability.kind == "unknown" and #delivered == 0)
prepare("unsupported", {kind = "Thresholds"}, {capability = "unsupported"})
assert(sent[2].outcome.error.code == "compaction_unsupported")
prepare("a", {kind = "Thresholds"}, {marker = "a"}); prepare("b", {kind = "Thresholds"}, {marker = "b"})
assert(#delivered == 1 and #sent == 2, "share catalog work, never caller IDs")
spec.from_plugin({{from = "p", body = {kind = "p.models.listed", models = {"m"}, context_windows = {m = 100}}}})
local seen = {}
for _, body in ipairs(sent) do if body.kind == "conversation.compaction.prepared" then seen[body.request_id] = body.outcome end end
assert(seen.a.kind == "Ready" and seen.b.kind == "Ready" and seen.a.regular_window == 100 and seen.b.regular_window == 100)
-- Successful response evidence is nonterminal and independent of occupancy/billing.
sent = {}
spec.to_plugin({{from = "conversation-manager", body = {kind = "conversation.provider.invoke", request_id = "native", provider = "p", approval_policy = "Default",
  conversation_id = "c", model = "m", watermark = 7}}})
spec.from_plugin({{from = "p", body = {kind = "p.completion.event", request_id = "native", event = "response_completed", provider = "p", model = "m"}}})
assert(sent[#sent].kind == "conversation.provider.event.reported" and sent[#sent].event == "response_completed")
assert(sent[#sent].latest_response_usage == nil and spec._internals.pending_requests().native ~= nil)
spec.from_plugin({{from = "p", body = {kind = "p.completion.event", request_id = "native", event = "failed", error = {code = "later_round_failed"}}}})
assert(sent[#sent].event == "failed" and spec._internals.pending_requests().native == nil)
sent, delivered = {}, {}
spec.to_plugin({pending("session-bound")})
assert(#delivered==2)
spec.to_plugin({{from="sessions",body={kind="sessions.session_start",session_id="new-session"}}})
assert(#sent==1 and sent[1].error.code=="compaction_session_changed")
assert(#delivered==4 and delivered[3].kind=="p.chat.cancel" and delivered[4].kind=="p.chat.delete")
spec.from_plugin({terminal("session-bound")})
assert(#sent==1,"session reset cannot leave a compactor that later publishes success")
nefor.engine = old_engine
print("provider_operation_test: all assertions passed")
