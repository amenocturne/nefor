package.path=table.concat({ROOT.."/lua/?.lua",ROOT.."/lua/?/init.lua",ROOT.."/plugins/chatgpt-provider/lua/?.lua",ROOT.."/plugins/chatgpt-provider/lua/?/init.lua",ROOT.."/plugins/openai-provider/lua/?.lua",ROOT.."/plugins/openai-provider/lua/?/init.lua",package.path},";")
local actor=require("core.actor")
local ncp=require("core.ncp")
function dispatch(log) ncp.dispatch(log) end
function invoke_from_plugin(source,payload) ncp.invoke_from_plugin(source,payload) end
actor.install()
require("libs.generic-provider").declare(); require("libs.generic-tool").declare()
local held_ack
local service=require("libs.conversation-manager.service").new()
actor.spawn(require("libs.conversation-manager.runtime").build{service=service,emit=function(payload)
  local body=nefor.json.decode(payload).body
  if not held_ack and body.kind=="conversation.fact.recorded" and body.event.sequence==1 then held_ack=payload; return end
  nefor.engine.send(payload)
  if body.kind=="conversation.context.notice.accepted" or body.kind=="conversation.context.snapshot" then nefor.engine.send(payload) end
  if held_ack and body.kind=="conversation.fact.recorded" and body.event.sequence==2 then nefor.engine.send(held_ack); held_ack=nil end
end})
actor.spawn(require("libs.compositors.provider").spawn_spec("gpt",{BIN.."/mock-plugin","--script",DIR.."/provider.lua","--lua-root",ROOT.."/lua"},{translator_lib=CASE=="unsupported" and "openai-provider" or "chatgpt-provider",tool_gate="tool-gate",agentic_loop={config=function() return {provider="gpt"} end},conversations=service:reader()}))
local tool=require("libs.tool")
actor.spawn(tool.actor{gate="tool-gate",source=tool.source{name="test-tools",tools={tool.define{name="echo",description="Deterministic sibling",parameters={type="object",properties={},additionalProperties=false},display={compact={label="echo"},expanded={label="echo",fields=nefor.json.mark_array({})},result={kind="content",fields=nefor.json.mark_array({})}},execute=function(args,call) call.ok("sibling done") end}}}})
actor.spawn(actor.identity_spec("tool-gate",{BIN.."/tool-gate","--auto","echo","--default","deny"}))
actor.spawn(actor.identity_spec("mag",{BIN.."/mag-plugin","--lua-root",ROOT.."/lua","--tool-gate","tool-gate"}))
local events={}
local kill_sent=false
local saved_result
local replay_conversation

local function emit(body) nefor.engine.send(nefor.json.encode{type="event",from="test-driver",ts=nefor.engine.now(),body=body}) end
actor.spawn{name="test-driver",receive_msg=function(entry)
  local env=nefor.json.decode(entry.payload); local b=env.body
  if type(b)~="table" then return end
  events[#events+1]={from=entry.origin,body=b}
  local f=assert(io.open(DIR.."/trace.jsonl","a")); f:write(nefor.json.encode(events[#events]),"\n"); f:close()
  if b.kind=="mag.hello" then
    emit{kind="mag.load",id="load",source_dir=DIR,entry="program.mag",module_roots={ROOT.."/mag/lib",ROOT.."/examples/nefor-agent/mag/lib"}}
  elseif b.kind=="mag.loaded" then
    local policy=not (CASE=="disabled" or CASE=="unsupported") and {soft_percent=60,step_percent=10,hard_percent=90} or nil
    emit{kind="mag.execute",id="execute",run_id="acceptance",run_name="acceptance",session_id="test-session",principal="lead",artifact=b.artifact,model_snapshot={provider="gpt",model="gpt-test",profiles={worker={provider="gpt",model="gpt-test",compaction_policy=policy}}}}
  elseif CASE=="cancel" and b.kind=="conversation.context.compact.accepted" and not kill_sent then
    kill_sent=true; emit{kind="mag.kill_run",id="kill",run_id="acceptance"}
  elseif b.kind=="mag.run_result" or b.kind=="mag.error" then
    if CASE=="soft" and b.kind=="mag.run_result" and b.status=="completed"
        and b.result and b.result.value and b.result.value.constructor=="Ok" then
      saved_result=b
      require("core.replay_window").set(true)
      emit{kind="sessions.session_start",session_id="replayed"}
      local replay_facts={}
      for _,e in ipairs(events) do
        if e.body.kind=="conversation.fact.recorded" then replay_facts[#replay_facts+1]=e.body end
      end
      table.sort(replay_facts,function(a,b) return a.event.sequence<b.event.sequence end)
      for _,fact in ipairs(replay_facts) do
        replay_conversation=fact.event.conversation_id
        nefor.engine.send(nefor.json.encode{type="event",from="conversation-manager",ts=nefor.engine.now(),replay=true,body=fact})
      end
      emit{kind="conversation.compaction.prepare.request",request_id="replay-prepare",provider="gpt",model="gpt-test",policy={soft_percent=60,step_percent=10,hard_percent=90}}
      emit{kind="test.replay.done"}
      return
    end
    local f=assert(io.open(DIR.."/result.json","w")); f:write(nefor.json.encode{events=events,result=b}); f:close()
    nefor.engine.shutdown{code=0,reason="compaction acceptance complete",grace_ms=1000}
  elseif b.kind=="test.replay.done" then
    local context=assert(service:reader():context(replay_conversation,{provider="gpt",model="gpt-test"}))
    assert(context.compaction and context.policy_state.epoch_id==context.compaction.request_id)
    assert(context.pending_compaction==nil)
    local f=assert(io.open(DIR.."/result.json","w")); f:write(nefor.json.encode{events=events,result=saved_result,replayed_context=context}); f:close()
    nefor.engine.shutdown{code=0,reason="compaction replay complete",grace_ms=1000}
  end
end}
