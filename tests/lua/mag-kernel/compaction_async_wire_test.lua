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
