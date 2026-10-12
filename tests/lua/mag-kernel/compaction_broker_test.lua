local Broker = require("compaction-broker")
local emitted, completions, results = {}, {}, {}
local live = true
local broker = Broker.new({ scope = "test/run", emit = function(b) emitted[#emitted+1] = b end,
  live = function() return live end, complete = function(_, c) if c then completions[#completions+1] = c end end })
local facet = broker.facet("actor")
local function done(r) results[#results+1] = r; return {status="ok"} end
local function fact(id, seq) return {kind="conversation.fact.recorded", event={event_id=id, conversation_id="c", sequence=seq}} end
broker.emit({event_id="a",conversation_id="c"})
broker.emit({event_id="b",conversation_id="c"})
facet.barrier({conversation_id="c"},done)
broker.event("conversation-manager",fact("b",8)); broker.pump(); assert(#results==0)
broker.event("wrong",fact("a",7)); broker.pump(); assert(#results==0)
broker.event("conversation-manager",fact("a",7)); assert(#results==0)
broker.pump(); assert(#results==1 and results[1].value.watermark==8 and #completions==1)
-- Duplicate recorded without any delta acknowledges the captured ID.
local duplicate = fact("a",7); duplicate.duplicate=true
broker.event("conversation-manager",duplicate); broker.pump(); assert(#results==1)
broker.emit({event_id="bad",conversation_id="c"})
broker.emit({event_id="later",conversation_id="c"})
facet.barrier({conversation_id="c"},done)
broker.event("conversation-manager",fact("later",10)); broker.pump(); assert(#results==1)
broker.event("conversation-manager",{kind="conversation.fact.rejected",event_id="bad",conversation_id="c",code="bad_write"})
broker.pump(); assert(results[2].error.code=="bad_write")
-- Later accepted writes cannot conceal the prior rejection.
facet.barrier({conversation_id="c"},done); broker.pump(); assert(results[3].error.code=="bad_write")
local read = facet.read({conversation_id="c",min_watermark=10},done)
assert(emitted[#emitted].request_id==read.id)
broker.event("wrong",{kind="conversation.context.snapshot",request_id=read.id,conversation_id="c",found=true,context={watermark=10}})
broker.pump(); assert(#results==3)
broker.event("conversation-manager",{kind="conversation.context.snapshot",request_id=read.id,conversation_id="other",found=true,context={watermark=10}})
broker.pump(); assert(#results==3)
broker.event("conversation-manager",{kind="conversation.context.snapshot",request_id=read.id,conversation_id="c",found=true,context={watermark=9}})
broker.pump(); assert(results[4].error.code=="conversation_watermark_stale")
local prep = facet.prepare({provider="p",model="m",policy={kind="Disabled"}},done)
local prepared={kind="conversation.compaction.prepared",provider="p",request_id=prep.id,outcome={kind="Ready",capability={kind="supported"}}}
broker.event("other",prepared); broker.pump(); assert(#results==4)
broker.event("p",prepared); broker.pump(); assert(results[5].value.capability.kind=="supported")
local notice = facet.bind_notice({conversation_id="c",request_id="native-request",notice_id="n"},done)
assert(emitted[#emitted].request_id=="native-request" and emitted[#emitted].command_id==notice.id)
broker.event("conversation-manager",{kind="conversation.context.notice.accepted",command_id=notice.id,request_id="native-request",conversation_id="c",operation="context_notice_included",receipt={kind="Accepted",notice_id="n"}})
broker.pump(); assert(results[6].value.kind=="Accepted")
local compact=facet.request_compaction({conversation_id="c",provider="p",model="m",epoch_id="old",history_id={1}},done)
local terminal={kind="conversation.fact.recorded",event={kind="context_compaction_completed",request_id=compact.id,conversation_id="c",provider="p",model="m",epoch_id="old",history_id={1},sequence=11}}
broker.event("conversation-manager",terminal); broker.pump(); assert(results[7].value.request_id==compact.id)
broker.event("conversation-manager",{kind="conversation.context.compact.accepted",request_id=compact.id,conversation_id="c"}); broker.pump(); assert(#results==7)
local compact2=facet.request_compaction({conversation_id="c",provider="p",epoch_id="old"},done)
broker.event("conversation-manager",{kind="conversation.context.compact.accepted",request_id=compact2.id,conversation_id="c"}); broker.pump(); assert(#results==7)
facet.cancel_compaction({request_id=compact2.id,conversation_id="c"},done)
terminal.event.request_id=compact2.id; broker.event("conversation-manager",terminal); broker.pump(); assert(#results==7)
broker.event("conversation-manager",{kind="conversation.context.compact.cancelled",request_id=compact2.id,conversation_id="c"});broker.pump();assert(#results==8)
local detached=facet.prepare({provider="p"},done);facet.detach(detached);prepared.request_id=detached.id;broker.event("p",prepared);broker.pump();assert(#results==8)
facet.barrier({conversation_id="fresh"},done);broker.detach_owner("actor");broker.pump();assert(#results==8)
facet.barrier({conversation_id="fresh"},done);live=false;broker.pump();assert(#results==8)

-- Detach after receipt but before pump also invalidates queued continuations.
live=true
local queued=facet.prepare({provider="p"},done);prepared.request_id=queued.id
broker.event("p",prepared);facet.detach(queued);broker.pump();assert(#results==8)
local interrupted=facet.request_compaction({conversation_id="c",provider="p",epoch_id="old"},done)
broker.interrupt("stop");broker.pump();assert(results[9].error.code=="compaction_interrupted")
assert(emitted[#emitted].kind=="conversation.context.compact.cancel" and emitted[#emitted].request_id==interrupted.id)
terminal.event.request_id=interrupted.id;broker.event("conversation-manager",terminal);broker.pump();assert(#results==9)

-- A rejection is not a shortcut around the remaining canonical write ACKs.
local isolated_results, isolated_emits = {}, {}
local isolated = Broker.new({scope="another-generation",emit=function(b) isolated_emits[#isolated_emits+1]=b end,
  live=function() return true end,complete=function() end})
local f = isolated.facet("worker")
local function result(r) isolated_results[#isolated_results+1]=r end
isolated.emit({event_id="first",conversation_id="c"})
isolated.emit({event_id="second",conversation_id="c"})
f.barrier({conversation_id="c"},result)
isolated.event("conversation-manager",{kind="conversation.fact.rejected",event_id="first",conversation_id="c",code="rejected"})
isolated.pump(); assert(#isolated_results==0)
isolated.event("conversation-manager",fact("second",2)); isolated.pump()
assert(#isolated_results==1 and isolated_results[1].error.code=="rejected")
-- Conflicting duplicate ACK cannot rewrite the first terminal decision.
isolated.event("conversation-manager",fact("first",3))
f.barrier({conversation_id="c"},result);isolated.pump()
assert(isolated_results[2].error.code=="rejected")

-- A canonical completion already queued is still detachable before its pump.
local operation=f.request_compaction({conversation_id="other",epoch_id="old",provider="p",model="m",history_id={1}},result)
local completed={kind="conversation.fact.recorded",event={kind="context_compaction_completed",request_id=operation.id,
  conversation_id="other",epoch_id="wrong",provider="p",model="m",history_id={1},sequence=4}}
isolated.event("conversation-manager",completed); isolated.pump(); assert(#isolated_results==2)
completed.event.epoch_id="old"
isolated.event("conversation-manager",completed)
local cancel=f.cancel_compaction({request_id=operation.id,conversation_id="other"},result)
assert(cancel.id~=operation.id and isolated_emits[#isolated_emits].request_id==operation.id)
isolated.pump(); assert(#isolated_results==2)
isolated.event("conversation-manager",{kind="conversation.context.compact.cancelled",request_id=operation.id,conversation_id="other"})
isolated.event("conversation-manager",{kind="conversation.context.compact.cancelled",request_id=operation.id,conversation_id="other"})
isolated.pump(); assert(#isolated_results==3)
isolated.event("conversation-manager",completed);isolated.pump();assert(#isolated_results==3)

-- Detaching a cancelled generation's old handle must not detach the cancel.
local next_operation=f.request_compaction({conversation_id="other"},result)
local next_cancel=f.cancel_compaction({request_id=next_operation.id,conversation_id="other"},result)
f.detach(next_operation)
isolated.event("conversation-manager",{kind="conversation.context.compact.cancelled",request_id=next_operation.id,conversation_id="other"})
isolated.pump();assert(#isolated_results==4)
-- Kill owns cancellation of pending compaction, not semantic rollback of reads.
f.request_compaction({conversation_id="other"},result)
f.read({conversation_id="other"},result)
isolated.detach_owner("worker");isolated.pump();assert(#isolated_results==4)
assert(isolated_emits[#isolated_emits].kind=="conversation.context.compact.cancel")
