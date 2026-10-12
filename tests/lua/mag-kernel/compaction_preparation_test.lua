local published, native = {}, {}
local context = {messages={},watermark=0}
nefor.engine = { now = function()return "t"end, send = function(payload)
  published[#published+1]=nefor.json.decode(payload).body
end }
local translator = {
  kinds={models_list_requested="p.models.list_requested",models_listed="p.models.listed"},
  compaction_capability=function(_, _, options)return {kind=options and options.capability or "supported",reason="adapter evidence"}end,
  publish=function(_,body)published[#published+1]=body end,
  deliver=function(body)native[#native+1]=body end,
  maybe_inject_static_token=function()end,
  outbound=function()return nil end,
  inbound=function()return nil end,
}
package.preload["bridge-test-translator"]=function()return {translator=function()return translator end}end
local spec = require("libs.compositors.provider").spawn_spec("p",{"unused"},{
  agentic_loop={},conversations={context=function()return context end},translator_lib="bridge-test-translator"})
local function request(id,policy,options)
  spec.to_plugin({{from="mag",body={kind="conversation.compaction.prepare.request",request_id=id,provider="p",model="m",policy=policy,provider_options=options}}})
end
request("disabled",{kind="Disabled"});assert(published[1].outcome.kind=="Ready" and published[1].outcome.capability.kind=="supported" and #native==0)
request("disabled-unknown",{kind="Disabled"},{capability="unknown"});assert(published[2].outcome.capability.kind=="unknown")
request("unsupported",{kind="Thresholds"},{capability="unsupported"});assert(published[3].outcome.error.code=="compaction_unsupported")
request("unknown",{kind="Thresholds"},{capability="unknown"});assert(published[4].outcome.error.code=="compaction_capability_unknown")
request("a",{kind="Thresholds"},{option="a"});request("b",{kind="Thresholds"},{option="b"})
assert(#native==1 and #published==4)
spec.from_plugin({{from="p",body={kind="p.models.listed",models={"m"},context_windows={m=100}}}})
local seen={}
for _,body in ipairs(published)do if body.kind=="conversation.compaction.prepared"then seen[body.request_id]=body.outcome end end
assert(seen.a.kind=="Ready" and seen.b.kind=="Ready" and seen.a.regular_window==100 and seen.b.regular_window==100)
request("known",{kind="Thresholds"});assert(published[#published].outcome.kind=="Ready" and #native==1)
spec.from_plugin({{from="p",body={kind="p.models.listed",models={"m"},context_windows={}}}})
request("missing",{kind="Thresholds"});assert(published[#published].outcome.error.code=="compaction_capacity_missing")
request("disabled-still",{kind="Disabled"});assert(published[#published].outcome.kind=="Ready")
-- Replay cannot trigger preparation or native execution.
local count=#published
spec.to_plugin({{replay=true,from="mag",body={kind="conversation.compaction.prepare.request",request_id="replay",provider="p",model="m"}}})
assert(#published==count and #native==1)

-- Inclusion is attested from submitted messages, not from the caller's arbitrary notice ID.
local strip=false
translator.complete=function(request,view)
  return {kind="native.complete",request_id=request.request_id,messages=strip and {} or view.messages}
end
context={messages={{id="notice-message",role="system",chunks={{kind="text",data="usage notice"}}}},watermark=7,history_id={1},
  policy_state={notices={n={message_id="notice-message",epoch_id="e"}}}}
local function invoke(id,notice)
  spec.to_plugin({{from="conversation-manager",body={kind="conversation.provider.invoke",provider="p",model="m",request_id=id,
    conversation_id="c",notice_id=notice,epoch_id="e",watermark=7}}})
end
local before=#native
invoke("included","n")
assert(#native==before+1 and native[#native].messages[1].chunks[1].data=="usage notice")
local found=false
for _,b in ipairs(published)do if b.kind=="conversation.provider.context_included" and b.request_id=="included"then found=true end end
assert(found)
strip=true;before=#native
invoke("omitted","n");assert(#native==before and published[#published].error.code=="notice_not_in_submitted_context")
invoke("fabricated","other");assert(#native==before and published[#published].error.code=="notice_not_in_submitted_context")
