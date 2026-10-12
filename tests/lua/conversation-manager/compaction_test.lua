local manager = require("libs.conversation-manager")
local projection = require("libs.conversation-manager.projection")
local policy = require("libs.compaction-policy")
local domain = manager.domain
local store, events, serial = manager.new(), {}, 0
local function eq(a, b) assert(domain.equal(a, b), "values differ: " .. tostring(a) .. " / " .. tostring(b)) end
local function add(kind, fields, error_code)
  serial = serial + 1
  local fact = domain.copy(fields or {})
  fact.kind, fact.event_id, fact.conversation_id = kind, "b:" .. serial, "b"
  local before = domain.copy(store:get("b"))
  local c, e, _, event = store:append(fact)
  if error_code then
    eq(c, nil); eq(e.code, error_code); eq(store:get("b"), before)
  else
    assert(c, e and e.code); events[#events + 1] = event
  end
  return c, event
end
local function context(pair) return projection.context(store:peek("b"), pair or {provider="p", model="m"}) end
local function message(id, role, text)
  add("message_started", {message_id=id, role=role, authored_prompt=role=="user" and text or nil})
  add("content_chunk_appended", {message_id=id, chunk={kind="text",data=text}})
  add("message_completed", {message_id=id})
end
local function binding(id)
  local c = context()
  return {notice_id=id, message_id=id..":message", epoch_id=c.policy_state.epoch_id,
    provider="p",model="m",history_id=c.history_id,expected_watermark=c.watermark,
    milestone_percent=80,measured_percent=81,total_tokens=81,regular_window=100,text="usage notice"}
end
local function request(id, cutoff)
  local c = context()
  return {request_id=id,provider="p",model="m",epoch_id=c.policy_state.epoch_id,
    history_id=c.history_id,context_watermark=c.watermark,history_cutoff=cutoff,trigger="manual"}
end
local function terminal(op, success)
  local f = domain.copy(op)
  if success then
    f.checkpoint={provider="p",model="m",format="opaque.test",artifact={data="checkpoint"}}
    f.compatibility={provider="p",model="m",format="opaque.test"}
  else f.error={code="compaction_interrupted",message="cancelled"} end
  return f
end
add("created", {provenance={provider="p",model="m"}})
message("sys", "system", "rules")
message("user", "user", "first")
local thresholds = assert(policy.validate({soft_percent=60,step_percent=10,hard_percent=90}))
eq(policy.preflight(thresholds, context().policy_state, {kind="completed_response",total_tokens=59},100).kind,"Allow")
local decision = policy.preflight(thresholds,context().policy_state,{kind="completed_response",total_tokens=81},100)
eq(decision.notice_data.milestone_percent,80)
local c, appended = add("context_notice_appended", binding("n"))
eq(context().messages[3].role,"system"); eq(#c.messages,3)
eq(context().policy_state.notified_through,nil)
eq(context().policy_state.delivered_notice_id,nil)
add("context_notice_appended", binding("duplicate"), "notice_pending")
local evidence = {notice_id="n",epoch_id=context().policy_state.epoch_id,provider="p",model="m",
  request_id="normal-1",history_id=context().history_id,context_watermark=context().watermark}
add("context_notice_delivered", evidence, "notice_request_mismatch")
add("context_notice_included", evidence)
eq(context().policy_state.notified_through,80)
eq(context().policy_state.delivered_notice_id,nil)
-- A failed response has no delivery fact; retry binds the SAME notice/system message.
local retry = domain.copy(evidence); retry.request_id="normal-2"; retry.context_watermark=context().watermark
add("context_notice_included", retry)
local stale = domain.copy(retry); stale.request_id="not-included"
add("context_notice_delivered", stale, "notice_request_mismatch")
stale=domain.copy(retry); stale.model="other"
add("context_notice_delivered", stale, "stale_notice_binding")
stale=domain.copy(retry); stale.epoch_id="past"
add("context_notice_delivered", stale, "stale_notice_binding")
-- Delivery is separate from measurement: a successful response without usage grants it.
add("context_notice_delivered", retry)
eq(context().policy_state.delivered_notice_id,"n")
local eligible = assert(policy.self_eligibility(thresholds,context().policy_state,
  {kind="supported",provider="p",model="m",history_id=context().history_id}))
eq(eligible.kind,"Allowed")
eq(context().latest_response_usage,nil)
eq(context().policy_state.outstanding_notice,nil)
eq(#context().messages,3)
eq(policy.preflight(thresholds,context().policy_state,{kind="completed_response",total_tokens=81},100).kind,"Allow")
add("response_usage_recorded", {provider="p",model="m",request_id="normal-2",
  latest_response_usage={kind="completed_response",total_tokens=81}})
eq(context().latest_response_usage.total_tokens,81)
eq(context({provider="p",model="other"}).latest_response_usage,nil)
local op=request("cancel-me",3)
add("context_compaction_requested",op)
add("context_compaction_requested", request("exclusive",3), "compaction_pending")
local frozen = projection.change(store:peek("b"),events[#events]).input
op.provider="wrong"
add("context_compaction_completed",terminal(op,true),"stale_compaction_binding")
op.provider="p"
add("context_compaction_failed",terminal(op,false))
eq(context().policy_state.delivered_notice_id,"n")
eq(context().latest_response_usage.total_tokens,81)
add("context_compaction_completed",terminal(op,true),"compaction_request_terminal")
-- Whole assistant exchange is excluded by the self prefix. A cutoff inside it is rejected.
add("message_started",{message_id="batch",role="assistant"})
add("tool_exchange_started",{message_id="batch",exchange_id="x"})
add("tool_call_completed",{exchange_id="x",call={id="self",name="self_compact",arguments={}}})
add("message_completed",{message_id="batch"})
add("context_compaction_requested",request("unsafe",4),"unsafe_history_cutoff")
local self=request("safe",3); self.trigger="self"; self.tool_call_id="self"
add("context_compaction_requested",self)
local input = projection.change(store:peek("b"),events[#events]).input
eq(#input.context.messages,3); eq(input.context.history_length,3)
add("context_compaction_completed",terminal(self,true))
eq(context().policy_state.epoch_id,"safe")
eq(context().policy_state.delivered_notice_id,nil)
eq(context().latest_response_usage,nil)
add("tool_result_recorded",{exchange_id="x",result={status="compacted"}})
add("message_started",{message_id="result",role="tool",tool_call_id="self"})
add("content_chunk_appended",{message_id="result",chunk={kind="text",data="result"}})
add("message_completed",{message_id="result"})
eq(#context().messages,5); eq(#context().tail_messages,2)
eq(context().tail_messages[1].tool_calls[1].id,context().tail_messages[2].tool_call_id)
eq(#input.context.messages,3); eq(#frozen.context.messages,3)
add("context_notice_delivered",retry,"stale_notice_binding")
local exact=request("exact",5)
add("context_compaction_requested",exact)
local outcome=terminal(exact,true)
outcome.optional_usage={kind="completed_response",total_tokens=7}
add("context_compaction_completed",outcome)
eq(context().latest_response_usage.total_tokens,7)
local later=request("short",2)
add("context_compaction_requested",later)
local prefix=projection.change(store:peek("b"),events[#events]).input.context
eq(prefix.compaction,nil,"a later checkpoint cannot restore into a shorter prefix")
add("context_compaction_failed",terminal(later,false))
local resumed=manager.new()
for _, event in ipairs(events) do assert(resumed:apply_recorded(domain.copy(event))) end
eq(projection.context(resumed:peek("b")),context(nil))
-- Rewind removes the notice and checkpoint on the abandoned branch, not their stored facts.
local head=domain.copy(store:peek("b").head)
local target=domain.copy(store:peek("b").messages[2].history_id)
add("rewind_committed",{target_history_id=target,expected_head=head})
eq(context().policy_state.epoch_id,"b:1"); eq(context().policy_state.delivered_notice_id,nil)
eq(context().compaction,nil)
eq(#store:get("b").messages,5)
add("context_notice_included",retry,"stale_notice_binding")
message("new","user","new branch")
local replacement=binding("replacement")
add("context_notice_appended",replacement)
eq(context().policy_state.outstanding_notice.notice_id,"replacement")
-- The live consumer facet acknowledges each accepted command. Resume never restarts pending work.
local runtime = require("libs.conversation-manager.runtime")
local service = require("libs.conversation-manager.service").new()
local emitted, replaying = {}, false
local actor = runtime.build({service=service,json=nefor.json,now=function() return "t" end,
  emit=function(payload) emitted[#emitted+1]=nefor.json.decode(payload).body end,
  replay_active=function() return replaying end})
local function receive(body)
  actor.receive_msg({payload=nefor.json.encode({from="conversation-manager",body=body})})
end
receive({kind="conversation.fact.append",fact={kind="created",event_id="live",conversation_id="live",provenance={provider="p",model="m"}}})
local facet=actor.compaction
local live=assert(facet.read({conversation_id="live",provider="p",model="m"}))
local receipt=assert(facet.append_notice({conversation_id="live",expected_watermark=live.watermark,
  notice_data={milestone_percent=80,measured_percent=81,total_tokens=81,regular_window=100},text="notice"}))
eq(receipt.kind,"Accepted")
local included=assert(facet.bind_notice({notice_id=receipt.notice_id,epoch_id=receipt.epoch_id,
  provider="p",model="m",request_id="live-response",context_watermark=receipt.watermark}))
eq(included.kind,"Accepted")
assert(facet.acknowledge_notice({notice_id=receipt.notice_id,epoch_id=receipt.epoch_id,
  provider="p",model="m",request_id="live-response",context_watermark=receipt.watermark}))
live=assert(facet.read({conversation_id="live",provider="p",model="m"}))
eq(live.policy_state.delivered_notice_id,receipt.notice_id)
local pending={conversation_id="live",request_id="pending",provider="p",model="m",epoch_id=receipt.epoch_id,
  history_id=live.history_id,context_watermark=live.watermark,history_cutoff=1,trigger="manual"}
assert(facet.request_compaction(pending))
local recorded
for _, body in ipairs(emitted) do
  if body.kind=="conversation.fact.recorded" and body.event.kind=="context_compaction_requested" then recorded=body.event end
end
assert(recorded)
receive({kind="conversation.provider.invoke.request",conversation_id="live",request_id="blocked",provider="p",model="m"})
eq(emitted[#emitted].code,"compaction_pending")
local all={}
for _, body in ipairs(emitted) do if body.kind=="conversation.fact.recorded" then all[#all+1]=body.event end end
service:writer():reset(); replaying=true; emitted={}
for _, event in ipairs(all) do receive({kind="conversation.fact.recorded",event=event}) end
for _, body in ipairs(emitted) do
  assert(body.kind~="conversation.provider.invoke")
  assert(not (body.change and body.change.kind=="context_compaction_pending"),"replay must not schedule a pending operation")
end
replaying=false
local recovered=assert(facet.read({conversation_id="live",provider="p",model="m"}))
eq(recovered.pending_compaction.request_id,"pending")
assert(facet.cancel_compaction({conversation_id="live",request_id="pending",reason="explicit resume recovery"}))
eq(facet.read({conversation_id="live",provider="p",model="m"}).pending_compaction,nil)
local duplicate,e=facet.cancel_compaction({conversation_id="live",request_id="pending"})
eq(duplicate,nil);eq(e.code,"compaction_request_not_pending")
eq(facet.read({conversation_id="live",provider="p",model="m"}).policy_state.delivered_notice_id,receipt.notice_id)
-- Native completion rejection closes canonical pending as failure, not success.
live=assert(facet.read({conversation_id="live",provider="p",model="m"}))
local rejected={conversation_id="live",request_id="reject-native",provider="p",model="m",
  epoch_id=live.policy_state.epoch_id,history_id=live.history_id,context_watermark=live.watermark,
  history_cutoff=1,trigger="manual"}
assert(facet.request_compaction(rejected))
local completion=terminal(rejected,true)
completion.kind="conversation.context.compact.complete"
completion.optional_usage={kind="completed_response",total_tokens=-1}
local wrong=domain.copy(completion); wrong.model="wrong"
receive(wrong)
eq(facet.read({conversation_id="live",provider="p",model="m"}).pending_compaction.request_id,"reject-native")
receive(completion)
local settled=assert(facet.read({conversation_id="live",provider="p",model="m"}))
eq(settled.pending_compaction,nil)
eq(settled.policy_state.epoch_id,live.policy_state.epoch_id)
eq(settled.policy_state.delivered_notice_id,receipt.notice_id)
local failed
for _, body in ipairs(emitted) do
  if body.kind=="conversation.projection.delta" and body.change.kind=="context_compaction_failed" then
    failed=body.change
  end
end
assert(failed); eq(failed.compaction.error.code,"compaction_commit_rejected")
eq(failed.compaction.error.message,"invalid_response_usage")
receive(completion) -- late duplicate cannot advance epoch or reopen pending
local late=assert(facet.read({conversation_id="live",provider="p",model="m"}))
eq(late.pending_compaction,nil); eq(late.policy_state.epoch_id,live.policy_state.epoch_id)
print("conversation_manager_compaction_test: all assertions passed")
