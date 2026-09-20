-- Lazy construction over the v4 typed topology. The topology buffers anonymous
-- assembly state; only a complete typed input reaches routing and constructs an
-- actor. Driven from engine/tests/starter_mag_kernel_test.rs.

local inventory = require("inventory")
local Registry = require("registry")
local routing = require("routing")
local observer = require("observer")
local Topology = require("topology")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function new_logger()
  local rec = { info = {}, warn = {}, error = {} }
  local function sink(bucket) return function(m) bucket[#bucket + 1] = m end end
  return { info = sink(rec.info), warn = sink(rec.warn), error = sink(rec.error) }, rec
end

local string_type = {kind="primitive",name="String"}
local string_pair_type = {kind="product",items={string_type,string_type}}

local function endpoint(id) return {constructor="ActorEndpoint",value={id=id}} end
local function port(id, wire, descriptor)
  return {endpoint=endpoint(id),wire=wire,type=descriptor,type_id=nefor.semantic_type.id(descriptor)}
end
local function actor(id, factory, input, outputs, params)
  return {id=id,factory=factory,type_arguments={},params=params or {},input=input,outputs=outputs or {}}
end
local function route(id, from, to, transforms)
  return {id=id,from=from,to=to,transforms=transforms or {}}
end
local function assemble(inputs, output, slot, path)
  return {constructor="Assemble",value={inputs=inputs,output=output,slot=slot,path=path}}
end
local function initial(to, descriptor, value)
  return {to=to,semantic_type=descriptor,semantic_type_id=nefor.semantic_type.id(descriptor),
    transforms={},content={kind=to.wire,value=value,semantic_value=value}}
end

-- Stand up the same typed topology -> routing -> lazy registry construction
-- chain as init.lua. `apply` performs topology preflight/install before the
-- actor fold, then injects typed initial messages after registration.
local function harness(factories)
  local log, rec = new_logger()
  local events, constructed = {}, {}
  local inv = inventory.new({log=log})
  local reg = Registry.new()
  for _, f in pairs(factories) do
    local make = f.make
    local _, err = reg:register({declaration=f.declaration,construct=function(id, params, emit)
      constructed[#constructed + 1] = id
      local inst = make(id, emit, params)
      inst.id = id
      emit({kind="mag.ready",from=id})
      return inst
    end})
    assert_true(err == nil, "factory registers: " .. tostring(err))
  end
  inv.registry = reg

  local topo
  local router = routing.new({
    inventory=inv,registry=reg,log=log,bus_emit=function() end,
    events=function(e) events[#events + 1] = e end,
    transform_route=function(sender, wire, arrival) topo:route(sender, wire, arrival) end,
    output_port=function(id, wire) return topo:output_port(id, wire) end,
  })
  topo = Topology.new({
    inventory=inv,semantic=nefor.semantic_type,
    dispatch=function(_, id, _, arrival) router:deliver(id, arrival) end,
    settle_result=function() return true end,
  })
  inv.set_on_kill(function(id) router:dispatch_kill(id); router:forget(id) end)
  inv.set_is_constructed(function(id) return router:is_constructed(id) end)
  router:set_construct(function(record)
    return reg:construct(record.factory, record.id, record.params, router:emitter(record.id), {})
  end)
  local obs = observer.new({inventory=inv,emit_event=function(e) events[#events + 1] = e end})

  local function apply(mod)
    mod = mod or {}
    mod.actors, mod.routes, mod.messages, mod.nodes, mod.kills =
      mod.actors or {}, mod.routes or {}, mod.messages or {}, mod.nodes or {}, mod.kills or {}
    local state, err = topo:preflight(mod)
    if not state then return {ok=false,error=err} end
    local snapshot = topo:snapshot()
    topo:install(mod, state)
    local actor_mod = {actors=mod.actors,kills=mod.kills,messages={}}
    local result = obs:apply(actor_mod)
    if not result.ok then topo:restore(snapshot); return result end
    for _, message in ipairs(mod.messages) do topo:initial(message) end
    return result
  end

  return {inv=inv,reg=reg,router=router,topo=topo,obs=obs,apply=apply,
    log=rec,events=events,constructed=constructed}
end

local function has_constructed(h, id)
  for _, c in ipairs(h.constructed) do if c == id then return true end end
  return false
end

local function event_kinds_for(h, id)
  local out = {}
  for _, e in ipairs(h.events) do if e.id == id then out[#out + 1] = e.kind end end
  return out
end

-- A product destination remains unconstructed after one assembly slot and
-- constructs exactly once when the second typed route completes the cohort.
do
  local fired = {}
  local h = harness({
    upstream={declaration={name="upstream",params={},inputs={input="seed.In"},outputs={"l.Out","r.Out"}},
      make=function(id, emit)
        return {deliver=function()
          local side = id == "u1" and "l.Out" or "r.Out"
          local value = id == "u1" and "left" or "right"
          emit({kind=side,from=id,value=value,semantic_value=value})
          return "ok"
        end}
      end},
    joiner={declaration={name="joiner",params={},inputs={joined="joined.In"},outputs={}},
      make=function() return {deliver=function(activation)
        fired[#fired + 1] = activation
        return "ok"
      end} end},
  })
  local u1_in, u2_in = port("u1","seed.In",string_type), port("u2","seed.In",string_type)
  local left, right = port("u1","l.Out",string_type), port("u2","r.Out",string_type)
  local joined = port("j","joined.In",string_pair_type)
  local actors = {
    actor("u1","upstream",u1_in,{left}), actor("u2","upstream",u2_in,{right}),
    actor("j","joiner",joined,{}),
  }
  local routes = {
    route("left-joined",left,joined,{assemble({string_type,string_type},string_pair_type,0,{"joined"})}),
    route("right-joined",right,joined,{assemble({string_type,string_type},string_pair_type,1,{"joined"})}),
  }
  local res = h.apply({actors=actors,routes=routes})
  assert_true(res.ok, "constellation registers: " .. tostring(res.error))
  assert_eq(#h.constructed, 0, "registration constructs nothing")

  res = h.apply({messages={initial(u1_in,string_type,"go")}})
  assert_true(res.ok, "first typed input applies: " .. tostring(res.error))
  assert_true(has_constructed(h,"u1"), "the activated source constructs")
  assert_true(not has_constructed(h,"j"), "a partial anonymous assembly does not construct")
  assert_eq(#fired, 0, "a partial assembly does not fire")

  res = h.apply({messages={initial(u2_in,string_type,"go")}})
  assert_true(res.ok, "second typed input applies: " .. tostring(res.error))
  assert_true(has_constructed(h,"j"), "the completing route constructs the joiner")
  assert_eq(#fired, 1, "one complete cohort fires exactly once")
  assert_eq(fired[1].shape, "product", "the assembled value retains its product input shape")
  assert_eq(#fired[1].messages, 1, "one assembled product reaches the actor")
  local value = fired[1].messages[1].message.value
  assert_eq(value[1], "left", "the first slot is retained")
  assert_eq(value[2], "right", "the second slot is retained")
end

-- Single and legacy-union factories still construct only on their first typed
-- input, after spawn, and reuse the same instance thereafter.
do
  local got = {}
  local h = harness({
    solo={declaration={name="solo",params={},inputs={input="seed.In"},outputs={}},
      make=function(id) return {deliver=function(a)
        got[#got + 1] = {id=id,n=a.messages[1].message.value}; return "ok"
      end} end},
    either={declaration={name="either",params={},inputs={boundary={"a.In","b.In"}},outputs={}},
      make=function(id) return {deliver=function(a)
        got[#got + 1] = {id=id,tag=a.messages[1].tag}; return "ok"
      end} end},
  })
  local solo_in, either_in = port("s","seed.In",string_type), port("u","b.In",string_type)
  local res = h.apply({
    actors={actor("s","solo",solo_in,{}),actor("u","either",either_in,{})},
    messages={initial(solo_in,string_type,"one"),initial(either_in,string_type,"two")},
  })
  assert_true(res.ok, "apply: " .. tostring(res.error))
  assert_true(has_constructed(h,"s"), "single constructs on its first message")
  assert_true(has_constructed(h,"u"), "union constructs on the arriving variant")
  assert_eq(#got,2,"both actors fired once")
  assert_eq(got[1].n,"one","the typed seed drove the first activation")
  assert_eq(got[2].tag,"b.In","the union activation retains the input wire")
  for _, id in ipairs({"s","u"}) do
    local kinds = event_kinds_for(h,id)
    assert_eq(kinds[1],"mag.actor_spawned",id .. ": spawned precedes ready")
    assert_eq(kinds[2],"mag.actor_ready",id .. ": ready follows first activation")
  end
  h.apply({messages={initial(solo_in,string_type,"two")}})
  local constructs = 0
  for _, c in ipairs(h.constructed) do if c == "s" then constructs = constructs + 1 end end
  assert_eq(constructs,1,"a later activation does not re-construct")
  assert_eq(got[3].n,"two","the second message reaches the existing instance")
end

-- Kill before construction drops the spec; later delivery and respawn remain
-- monotone no-ops and never manufacture an instance.
do
  local killed_handler_ran, delivered = false, 0
  local h = harness({idle={
    declaration={name="idle",params={},inputs={input="seed.In"},outputs={},signals={"kill"}},
    make=function() return {
      deliver=function() delivered=delivered+1; return "ok" end,
      handle_kill=function() killed_handler_ran=true end,
    } end,
  }})
  local input = port("x","seed.In",string_type)
  local spec = actor("x","idle",input,{})
  local res = h.apply({actors={spec}})
  assert_true(res.ok,"spawn x: " .. tostring(res.error))
  assert_eq(h.inv.state_of("x"),"alive","registered actor counts as alive")
  assert_eq(#h.constructed,0,"no construction without activation")
  local k = h.apply({kills={"x"}})
  assert_true(k.ok,"kill applies")
  assert_eq(h.inv.state_of("x"),"dead","the spec drops to a tombstone")
  assert_true(not killed_handler_ran,"no courtesy kill delivery without an instance")
  local kinds = event_kinds_for(h,"x")
  assert_eq(kinds[1],"mag.actor_spawned","spawned fired at registration")
  assert_eq(kinds[2],"mag.actor_killed","killed still fires for observability")
  assert_eq(#kinds,2,"no ready ever fired")
  local s = h.apply({messages={initial(input,string_type,"late")}})
  assert_true(s.ok,"a dead-target typed send is a no-op")
  assert_eq(delivered,0,"nothing reaches the dead actor")
  assert_eq(#h.constructed,0,"dead-target delivery does not construct")
  local r = h.apply({actors={spec}})
  assert_true(r.ok,"respawn-after-kill is a monotone no-op")
  assert_eq(h.inv.state_of("x"),"dead","spawn cannot revive a dead id")
  assert_eq(#h.constructed,0,"the no-op respawn constructs nothing")
end

-- A later typed delta may add a producer and route to an actor which was
-- registered by an earlier modification. The existing destination remains
-- lazy until that delta's initial message actually produces a value.
do
  local received = {}
  local h = harness({
    producer={declaration={name="producer",params={},inputs={input="seed.In"},outputs={"value.Out"}},
      make=function(id,emit) return {deliver=function()
        emit({kind="value.Out",from=id,value="delta",semantic_value="delta"}); return "ok"
      end} end},
    consumer={declaration={name="consumer",params={},inputs={input="value.In"},outputs={}},
      make=function() return {deliver=function(activation)
        received[#received + 1] = activation.messages[1].message.value; return "ok"
      end} end},
  })
  local consumer_in = port("consumer","value.In",string_type)
  local res = h.apply({actors={actor("consumer","consumer",consumer_in,{})}})
  assert_true(res.ok,"existing consumer registers: " .. tostring(res.error))
  assert_true(not has_constructed(h,"consumer"),"the existing consumer starts unconstructed")

  local producer_in = port("producer","seed.In",string_type)
  local producer_out = port("producer","value.Out",string_type)
  res = h.apply({
    actors={actor("producer","producer",producer_in,{producer_out})},
    routes={route("delta-to-existing",producer_out,consumer_in)},
    messages={initial(producer_in,string_type,"go")},
  })
  assert_true(res.ok,"delta route to existing actor applies: " .. tostring(res.error))
  assert_true(has_constructed(h,"consumer"),"the delta-produced value constructs the existing actor")
  assert_eq(received[1],"delta","the existing actor receives the routed delta value")
end

-- A completed path does not construct a routed alternative which never emits.
do
  local h = harness({
    entry={declaration={name="entry",params={},inputs={input="seed.In"},outputs={"hop.Ping","alt.Out"}},
      make=function(id,emit) return {deliver=function()
        emit({kind="hop.Ping",from=id,value="done",semantic_value="done"}); return "ok"
      end} end},
    terminal={declaration={name="terminal",params={},inputs={final="hop.Ping"},outputs={}},
      make=function(id,emit) return {deliver=function()
        emit({kind="mag.RunComplete",from=id,result={text="done"},persisted=false}); return "ok"
      end} end},
    summarizer={declaration={name="summarizer",params={},inputs={input="alt.Out"},outputs={}},
      make=function() return {deliver=function() return "ok" end} end},
  })
  local entry_in = port("entry","seed.In",string_type)
  local happy, alternate = port("entry","hop.Ping",string_type), port("entry","alt.Out",string_type)
  local sink_in, exhaust_in = port("sink","hop.Ping",string_type), port("exhaust","alt.Out",string_type)
  local res = h.apply({
    actors={actor("entry","entry",entry_in,{happy,alternate}),actor("sink","terminal",sink_in,{}),actor("exhaust","summarizer",exhaust_in,{})},
    routes={route("happy",happy,sink_in),route("alternate",alternate,exhaust_in)},
    messages={initial(entry_in,string_type,"go")},
  })
  assert_true(res.ok,"apply: " .. tostring(res.error))
  local completed = false
  for _, e in ipairs(h.events) do if e.kind == "mag.run_complete" then completed = true end end
  assert_true(completed,"the run completed")
  assert_true(has_constructed(h,"entry"),"entry constructed")
  assert_true(has_constructed(h,"sink"),"sink constructed")
  assert_true(not has_constructed(h,"exhaust"),"the never-activated actor never constructs")
  local kinds = event_kinds_for(h,"exhaust")
  assert_eq(kinds[1],"mag.actor_spawned","the alternate consumer was registered")
  for _, kind in ipairs(kinds) do assert_true(kind ~= "mag.actor_ready","the alternate never readied") end
  assert_eq(h.inv.state_of("exhaust"),"alive","the unconstructed actor still counts as alive")
end

print("mag-kernel lazy_construct_test: all cases passed")
