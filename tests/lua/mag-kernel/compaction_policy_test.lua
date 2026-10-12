local policy = require("libs.compaction-policy")
local snapshot = require("libs.model-snapshot")
local raw = {soft_percent=60,step_percent=10,hard_percent=90}
local p = assert(policy.validate(raw))
assert(policy.validate(nil).kind == "Disabled")
assert(policy.validate(nefor.json.decode("null")).kind == "Disabled")
assert(policy.validate({soft_percent=60.5,step_percent=0.25,hard_percent=90.5}))
for _, invalid in ipairs({false, {}, {60,10,90}, {soft_percent=0,step_percent=10,hard_percent=90},
  {soft_percent=90,step_percent=10,hard_percent=90}, {soft_percent=60,step_percent=0,hard_percent=90},
  {soft_percent=60,step_percent=10,hard_percent=101}, {soft_percent=0/0,step_percent=10,hard_percent=90},
  {soft_percent=60,step_percent=math.huge,hard_percent=90},
  {soft_percent=60,step_percent=10,hard_percent=90,extra=1}}) do
  assert(policy.validate(invalid) == nil)
end
local state = {epoch_id="created"}
local function decision(total, s)
  return assert(policy.preflight(p,s or state,{kind="completed_response",total_tokens=total},1000))
end
assert(decision(0).kind == "Allow")
assert(policy.preflight(p,state,nil,1000).kind == "Allow")
assert(policy.preflight(p,state,{kind="billing",total_tokens=950},1000).kind == "Allow")
assert(policy.preflight(p,state,{kind="completed_response",input_tokens=950},1000).kind == "Allow")
assert(decision(599).kind == "Allow")
assert(decision(600).notice_data.milestone_percent == 60)
assert(decision(810).notice_data.milestone_percent == 80)
assert(decision(899).kind == "Notice")
assert(decision(900).kind == "Compact")
local tiny = assert(policy.validate({soft_percent=60,step_percent=1e-300,hard_percent=90}))
assert(policy.preflight(tiny,state,{kind="completed_response",total_tokens=810},1000).kind == "Notice")
assert(decision(1200).kind == "Compact")
assert(state.notified_through == nil, "preflight has no effects")
local appended={kind="context_notice_appended",notice_id="notice",epoch_id="created",message_id="message",
  provider="p",model="m",history_id={1},milestone_percent=80}
state=policy.reduce(state,appended)
assert(state.notified_through == nil)
assert(decision(810,state).kind == "Allow", "outstanding notice coalesces retries")
local scope={kind="supported",provider="p",model="m",history_id={1}}
assert(policy.self_eligibility(p,state,scope) == nil)
local included={kind="context_notice_included",notice_id="notice",epoch_id="created",provider="p",model="m",history_id={1},request_id="r1"}
state=policy.reduce(state,included)
assert(state.notified_through == 80)
included.request_id="r2"
state=policy.reduce(state,included)
assert(#state.notices.notice.request_ids == 2)
assert(policy.self_eligibility(p,state,scope) == nil, "inclusion is not delivery")
local delivered={kind="context_notice_delivered",notice_id="notice",epoch_id="created",provider="p",model="m",history_id={1},request_id="other"}
assert(policy.reduce(state,delivered).delivered_notice_id == nil)
delivered.request_id="r2"
state=policy.reduce(state,delivered)
assert(policy.self_eligibility(p,state,scope).kind == "Allowed")
assert(state.outstanding_notice == nil)
assert(decision(810,state).kind == "Allow")
assert(decision(610,state).kind == "Allow")
scope.model="other"
assert(policy.self_eligibility(p,state,scope) == nil)
scope.model="m"; scope.history_id={2}
assert(policy.self_eligibility(p,state,scope) == nil)
local failed=policy.reduce(state,{kind="context_compaction_failed",request_id="c"})
assert(failed.epoch_id == "created" and failed.delivered_notice_id == "notice")
local reset=policy.reduce(state,{kind="context_compaction_completed",request_id="c"})
assert(reset.epoch_id == "c" and reset.notified_through == nil and reset.delivered_notice_id == nil)
assert(policy.self_eligibility(policy.validate(nil),state,{kind="unsupported"}) == nil)
local copied=assert(snapshot.copy({provider="p",model="m",compaction_policy=raw,
  profiles={current={provider="p",model="m",compaction_policy=raw}}}))
raw.soft_percent=61
assert(copied.compaction_policy.soft_percent == 60 and copied.profiles.current.compaction_policy.soft_percent == 60)
assert(snapshot.copy({provider="p",model="m",compaction_policy={}}) == nil)
assert(snapshot.copy({provider="p",model="m",profiles={current={provider="p",model="m",compaction_policy={}}}}) == nil)
assert(snapshot.copy({provider="p",model="m",compaction_policy=nefor.json.decode("null")}).compaction_policy == nil)
print("compaction pure policy assertions passed")

assert(policy.lower({constructor="Thresholds"}) == nil)
assert(policy.lower({constructor="Thresholds",value=nefor.json.decode("null")}) == nil)
assert(policy.lower({constructor="Disabled",value={}}) == nil)
assert(policy.lower({constructor="Disabled",value=nefor.json.decode("null")}).kind == "Disabled")
assert(policy.lower({constructor="Thresholds",value={soft_percent=60,step_percent=10,hard_percent=90}}).kind == "Thresholds")
local options = {nested={value="preserved"}}
local source = {provider="p",model="m",provider_options=options,profiles={worker={provider="p",model="m",provider_options=options,compaction_policy={soft_percent=60,step_percent=10,hard_percent=90}}}}
local copy = assert(snapshot.copy(source))
assert(copy.compaction_policy == nil)
options.nested.value = "changed"
source.profiles.worker.compaction_policy.soft_percent = 61
assert(copy.provider_options.nested.value == "preserved")
assert(copy.profiles.worker.provider_options.nested.value == "preserved")
assert(copy.profiles.worker.compaction_policy.soft_percent == 60)
assert(snapshot.copy({provider="p",model="m",profiles={worker={provider="p",model="m",compaction_policy=nefor.json.decode("[]")}}}) == nil)
