local service = require("libs.conversation-manager.service").new()
local emitted, delivered, providers = {}, {}, {}
local function capture(payload) emitted[#emitted+1] = nefor.json.decode(payload) end
local manager = require("libs.conversation-manager.runtime").build{
  service=service, emit=capture, now=function() return "test" end,
  replay_active=function() return false end,
}
nefor.engine = {
  now=function() return "test" end,
  send=capture,
  deliver=function(name,payload) delivered[#delivered+1]=nefor.json.decode(payload).body end,
}
local function provider(name, translator)
  if not providers[name] then
    providers[name]=require("libs.compositors.provider").spawn_spec(name, {}, {
      translator_lib=translator or "openai-provider", tool_gate="tool-gate",
      agentic_loop={config=function() return {provider=name} end},
      conversations=service:reader(),
    })
  end
  return providers[name]
end
function canonical_test_handle(source, body, translator)
  emitted, delivered = {}, {}
  local env={type="event",from=source,ts="test",body=body}
  manager.receive_msg{origin=source,payload=nefor.json.encode(env)}
  if body.kind=="conversation.compaction.prepare.request" then
    provider(body.provider,translator).to_plugin({env})
  end
  local cursor=1
  while cursor<=#emitted do
    local event=emitted[cursor]; cursor=cursor+1
    if event.body.kind=="conversation.provider.invoke" then
      provider(event.body.provider,translator).to_plugin({event})
    end
    assert(event.body.kind~="conversation.fact.rejected",nefor.json.encode(event.body))
    assert(event.body.kind~="conversation.query.rejected",nefor.json.encode(event.body))
  end
  return {events=nefor.json.mark_array(emitted),delivered=nefor.json.mark_array(delivered)}
end
