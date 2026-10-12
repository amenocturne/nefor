local name = "gpt"
local count = 0
local function witness(kind, body)
  local f=assert(io.open(DIR.."/native.jsonl","a")); f:write(nefor.json.encode{kind=kind,body=body},"\n"); f:close()
end
local function event(id, event, fields)
  fields = fields or {}
  fields.request_id=id; fields.event=event; fields.provider=name; fields.model="gpt-test"
  nefor.emit_raw(name..".completion.event",fields)
end
nefor.on_ready_ok(function() nefor.emit_raw("gpt.hello",{}) end)
nefor.on("gpt.models.list_requested",function()
  nefor.emit_raw("gpt.models.listed",{models={"gpt-test"},context_windows=CASE=="missing" and {} or { ["gpt-test"]=100 },max_context_windows={ ["gpt-test"]=1000 }})
end)
nefor.on("gpt.completion.request",function(body)
  count=count+1
  witness("request",{count=count,request=body})
  local id=body.request_id
  if count==1 then
    event(id,"usage",{latest_response_usage={kind="completed_response",total_tokens=CASE=="hard" and 95 or 65,response_id="round1"}})
    event(id,"response_completed",{response_id="round1"})
    event(id,"completed",{result={text="",tool_calls={{id="probe",name="self_compact",arguments={}}},finish_reason="tool_calls"}})
  elseif count==2 and CASE=="hard" then
    event(id,"completed",{result={text="done",finish_reason="stop"}})
  elseif count==2 then
    event(id,"response_completed",{response_id="round2"})
    if CASE=="partial" then event(id,"error",{message="later native round failed"}); return end
    event(id,"completed",{result={text="",tool_calls={{id="self1",name="self_compact",arguments={}},{id="sibling",name="echo",arguments={}},{id="self2",name="self_compact",arguments={}}},finish_reason="tool_calls"}})
  elseif count==3 and (CASE=="structured" or CASE=="dynamic") then
    event(id,"completed",{result={tool_calls={{id="write",name="write_output",arguments={new_string=CASE=="dynamic" and '["item"]' or '{"answer":"done"}',validate=true}}},finish_reason="tool_calls"}})
  elseif count==4 and (CASE=="structured" or CASE=="dynamic") then
    event(id,"completed",{result={tool_calls={{id="submit",name="submit_output",arguments={}}},finish_reason="tool_calls"}})
  else
    event(id,"response_completed",{response_id="round3"})
    event(id,"completed",{result={text="done",finish_reason="stop"}})
  end
end)
nefor.on("gpt.chat.append",function(body) witness("frozen_message",body) end)
nefor.on("gpt.chat.compact",function(body)
  witness("compact",body)
  if CASE=="cancel" then return end
  nefor.emit_raw("gpt.chat.compaction.commit",{chat_id=body.chat_id,model="gpt-test",model_context_artifact={items={{type="compaction",encrypted_content="opaque-native-test"}}}})
end)

nefor.on("gpt.chat.cancel",function(body)
  witness("cancel",body)
  nefor.emit_raw("gpt.chat.compaction.commit",{chat_id=body.chat_id,model="gpt-test",model_context_artifact={items={{type="compaction",encrypted_content="late"}}}})
end)
