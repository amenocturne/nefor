local runtime = require("libs.conversation-manager.runtime")
local service = require("libs.conversation-manager.service").new()
local emitted = {}
local actor = runtime.build({json=nefor.json,service=service,now=function()return "t"end,
  replay_active=function()return false end,emit=function(payload)emitted[#emitted+1]=nefor.json.decode(payload).body end})
local function receive(body)
  actor.receive_msg({origin="mag",payload=nefor.json.encode({type="event",from="mag",body=body})})
end
local function last()return emitted[#emitted]end
receive({kind="conversation.fact.append",fact={kind="created",event_id="created",conversation_id="wire",provenance={provider="p",model="m"}}})
receive({kind="conversation.context.request",request_id="read",conversation_id="wire",min_watermark=999})
assert(last().kind=="conversation.query.rejected" and last().request_id=="read" and last().code=="conversation_watermark_stale")
receive({kind="conversation.context.request",request_id="read2",conversation_id="wire",min_watermark=1})
assert(last().kind=="conversation.context.snapshot" and last().context.watermark==1)
receive({kind="conversation.context.notice.bind",command_id="bind",request_id="native",conversation_id="missing"})
assert(last().kind=="conversation.context.notice.rejected" and last().command_id=="bind" and last().request_id=="native")
receive({kind="conversation.context.notice.append",command_id="append",conversation_id="wire",expected_watermark=99})
assert(last().kind=="conversation.context.notice.rejected" and last().command_id=="append")
receive({kind="conversation.context.compact.request",request_id="compact",conversation_id="missing",provider="p"})
assert(last().kind=="conversation.context.compact.rejected" and last().request_id=="compact")
receive({kind="conversation.provider.invoke.request",request_id="invoke",conversation_id="wire",provider="p",context_watermark=999})
assert(last().kind=="conversation.provider.invoke.rejected" and last().code=="conversation_watermark_stale")
-- Exact producer schema: command_id is RPC correlation, request_id is native evidence.
receive({kind="conversation.context.request",request_id="view",conversation_id="wire",provider="p",model="m",min_watermark=1})
local context = last().context
receive({kind="conversation.context.notice.append",command_id="append-ok",conversation_id="wire",
  expected_watermark=context.watermark,epoch_id=context.policy_state.epoch_id,history_id=context.history_id,
  provider="p",model="m",notice_data={milestone_percent=80,measured_percent=81,total_tokens=81,regular_window=100},text="notice"})
assert(last().kind=="conversation.context.notice.accepted" and last().command_id=="append-ok")
local receipt = last().receipt
assert(last().request_id==nil and receipt.kind=="Accepted")
receive({kind="conversation.context.request",request_id="notice-view",conversation_id="wire",provider="p",model="m",min_watermark=receipt.watermark})
context = last().context
receive({kind="conversation.context.notice.bind",command_id="bind-ok",conversation_id="wire",notice_id=receipt.notice_id,
  epoch_id=receipt.epoch_id,request_id="actual-native-id",provider="p",model="m",history_id=context.history_id,context_watermark=context.watermark})
assert(last().kind=="conversation.context.notice.accepted" and last().command_id=="bind-ok" and last().request_id=="actual-native-id")
-- Acknowledge has no invented watermark; manager takes it from the matching accepted inclusion.
receive({kind="conversation.context.notice.acknowledge",command_id="ack-ok",conversation_id="wire",notice_id=receipt.notice_id,
  epoch_id=receipt.epoch_id,request_id="actual-native-id",provider="p",model="m",history_id=context.history_id,response_id="round"})
assert(last().kind=="conversation.context.notice.accepted" and last().command_id=="ack-ok" and last().request_id=="actual-native-id")
local watermark = last().receipt.watermark
receive({kind="conversation.context.notice.acknowledge",command_id="ack-duplicate",conversation_id="wire",notice_id=receipt.notice_id,
  epoch_id=receipt.epoch_id,request_id="actual-native-id",provider="p",model="m",history_id=context.history_id,response_id="round"})
assert(last().kind=="conversation.context.notice.accepted" and last().command_id=="ack-duplicate" and last().duplicate)
assert(last().receipt.watermark==watermark)
local function snapshot()
  receive({kind="conversation.context.request",request_id="snapshot",conversation_id="wire",provider="p",model="m",min_watermark=watermark})
  return last().context
end
context = snapshot()
assert(context.policy_state.delivered_notice_id==receipt.notice_id)
local compact = {kind="conversation.context.compact.request",request_id="canonical-operation",conversation_id="wire",
  epoch_id=context.policy_state.epoch_id,history_id=context.history_id,context_watermark=context.watermark,
  history_cutoff=1,provider="p",model="m",trigger="manual"}
receive(compact)
assert(last().kind=="conversation.context.compact.accepted" and last().request_id==compact.request_id)
context = snapshot()
assert(context.pending_compaction.request_id==compact.request_id and context.policy_state.epoch_id~=compact.request_id)
receive({kind="conversation.context.compact.cancel",request_id=compact.request_id,conversation_id="wire",reason="cancel"})
assert(last().kind=="conversation.context.compact.cancelled" and last().request_id==compact.request_id)
context = snapshot()
assert(context.pending_compaction==nil and context.policy_state.delivered_notice_id==receipt.notice_id)
-- Late raw success cannot override the canonical failed operation.
receive({kind="conversation.context.compact.complete",request_id=compact.request_id,conversation_id="wire",
  epoch_id=compact.epoch_id,history_id=compact.history_id,provider="p",model="m",
  checkpoint={provider="p",model="m",format="opaque.v1",artifact="late"},compatibility={provider="p",model="m",format="opaque.v1"}})
assert(snapshot().policy_state.epoch_id==compact.epoch_id)
local view, failure = service:reader():read({conversation_id="wire",min_watermark=999})
assert(view==nil and failure.code=="conversation_watermark_stale")
print("conversation_manager_async_wire_test: all assertions passed")
