use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde_json::json;
use std::path::PathBuf;

fn harness() -> Lua {
    let lua = Lua::new();
    let globals = lua.globals();
    let package: Table = globals.get("package").unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel");
    let current: String = package.get("path").unwrap();
    let shared = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lua");
    package
        .set(
            "path",
            format!(
                "{}/?.lua;{}/?/init.lua;{}/?.lua;{}/?/init.lua;{current}",
                root.display(),
                root.display(),
                shared.display(),
                shared.display()
            ),
        )
        .unwrap();
    lua.load(r#"
      local function type_id(value)
        if value.kind == "primitive" then return value.name end
        if value.kind == "list" then return "List<" .. type_id(value.item) .. ">" end
        if value.kind == "product" then
          local ids={}; for i,item in ipairs(value.items) do ids[i]=type_id(item) end
          return "(" .. table.concat(ids, ",") .. ")"
        end
        return value.name
      end
      local function validate(descriptor,value)
        if descriptor.kind=="primitive" then
          if descriptor.name=="Unit" then return value==nil end
          if descriptor.name=="String" then return type(value)=="string" end
          if descriptor.name=="Int" then return type(value)=="number" and value%1==0 end
          if descriptor.name=="Bool" then return type(value)=="boolean" end
        elseif descriptor.kind=="list" then
          if type(value)~="table" then return false end
          for _,item in ipairs(value) do if not validate(descriptor.item,item) then return false end end
          return true
        elseif descriptor.kind=="product" then
          if type(value)~="table" or #value~=#descriptor.items then return false end
          for i,item in ipairs(descriptor.items) do if not validate(item,value[i]) then return false end end
          return true
        elseif descriptor.kind=="adt" then
          if type(value)~="table" then return false end
          for _,c in ipairs(descriptor.constructors or {}) do
            if c.name==value.constructor then return validate(c.payload,value.value) end
          end
          return false
        end
        return true
      end
      local function input_covered_by(target,sources)
        if #sources>0 then
          local whole=true
          for _,source in ipairs(sources) do if type_id(target)~=type_id(source) then whole=false;break end end
          if whole then return true end
        end
        if target.kind~="product" then return #sources>0 end
        if #sources~=#target.items then return false end
        local assigned={}
        for _,source in ipairs(sources) do
          local found
          for index,item in ipairs(target.items) do
            if not assigned[index] and type_id(item)==type_id(source) then found=index;break end
          end
          if not found then return false end
          assigned[found]=true
        end
        return true
      end
      nefor={json={encode=function(v)
        if type(v)~="table" then return tostring(v) end
        local parts={}; for k,x in pairs(v) do parts[#parts+1]=tostring(k)..":"..tostring(x) end
        table.sort(parts); return table.concat(parts,"|")
      end,mark_array=function(v)return v end},semantic_type={id=type_id,
        accepts=function(a,b)return type_id(a)==type_id(b) or
          (a.kind=="product" and (function()
            for _,item in ipairs(a.items) do if type_id(item)==type_id(b) then return true end end
            return false
          end)()) end,
        input_covered_by=input_covered_by,
        validate_value=function(d,v)return {ok=validate(d,v)} end,
        constructor=function(owner,name)
          for _,c in ipairs(owner.constructors or {}) do if c.name==name then
            return {id=owner.name.."."..name,payload=c.payload,payload_id=type_id(c.payload)} end end
          error("unknown constructor")
        end}}
    "#).exec().unwrap();
    lua
}

#[test]
fn ordered_route_transforms_preserve_fanout_and_branch_isolation() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}; local integer={kind="primitive",name="Int"}
      local pair={kind="product",items={string,integer}}
      local either={kind="adt",name="test.Either",constructors={{name="Left",payload=string},{name="Right",payload=integer}}}
      local function e(id)return {constructor="ActorEndpoint",value={id=id}} end
      local function p(id,wire,t)return {endpoint=e(id),wire=wire,type=t,type_id=nefor.semantic_type.id(t)} end
      local actors={}
      local function actor(id,input,outputs) local a={id=id,input=input,outputs=outputs}; actors[id]=a; return a end
      local source=actor("source",p("source","in",pair),{p("source","out",pair)})
      local left=actor("left",p("left","in",string),{})
      local right=actor("right",p("right","in",integer),{})
      local seen={}; local source_arrival
      local topo=Topology.new({inventory={pairs=function()return pairs(actors)end},semantic=nefor.semantic_type,
        settle_result=function()end,dispatch=function(_,id,_,arrival)
          if id=="source" then source_arrival=arrival else seen[#seen+1]={id=id,value=arrival.payload.value} end
        end})
      local routes={
        {id="left",from=source.outputs[1],to=left.input,transforms={
          {constructor="Project",value={input=pair,output=string,index=0}},
          {constructor="Pack",value={owner=either,payload=string,constructor="Left"}},
          {constructor="Unpack",value={owner=either,payload=string,constructor="Left"}}}},
        {id="wrong-branch",from=source.outputs[1],to=right.input,transforms={
          {constructor="Project",value={input=pair,output=string,index=0}},
          {constructor="Pack",value={owner=either,payload=string,constructor="Left"}},
          {constructor="Unpack",value={owner=either,payload=integer,constructor="Right"}}}},
        {id="right",from=source.outputs[1],to=right.input,transforms={{constructor="Project",value={input=pair,output=integer,index=1}}}},
      }
      local mod={actors={source,left,right},routes=routes,messages={},nodes={},kills={}}
      local state,err=topo:preflight(mod); assert(state,err); topo:install(mod,state)
      topo:initial({to=source.input,transforms={},semantic_type=pair,semantic_type_id=nefor.semantic_type.id(pair),content={value={"x",7},semantic_value={"x",7}}})
      topo:route("source","out",source_arrival)
      return seen
    "#).eval().unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!([
            {"id":"left","value":"x"}, {"id":"right","value":7}
        ])
    );
}

#[test]
fn destination_owned_assemblies_use_explicit_fifo_slots() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}; local pair={kind="product",items={string,string}}
      local function e(id)return {constructor="ActorEndpoint",value={id=id}} end
      local function p(id,w,t)return {endpoint=e(id),wire=w,type=t,type_id=nefor.semantic_type.id(t)} end
      local actors={dest={id="dest",input=p("dest","in",pair),outputs={}}}
      local seen={}
      local topo=Topology.new({inventory={pairs=function()return pairs(actors)end},semantic=nefor.semantic_type,
        settle_result=function()end,dispatch=function(_,id,_,arrival) seen[#seen+1]=arrival.payload.value end})
      local function send(slot,value,path)
        topo:initial({to=actors.dest.input,semantic_type=string,semantic_type_id="String",content={value=value,semantic_value=value},
          transforms={{constructor="Assemble",value={inputs={string,string},output=pair,slot=slot,path=path,kind="product"}}}})
      end
      send(0,"L1",{0}); send(0,"L2",{0}); send(1,"R1",{0}); send(1,"R2",{0})
      send(1,"B-right",{1}); send(0,"B-left",{1})
      return seen
    "#).eval().unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!([["L1", "R1"], ["L2", "R2"], ["B-left", "B-right"]])
    );
}

#[test]
fn one_producer_can_feed_multiple_equal_typed_slots() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}; local pair={kind="product",items={string,string}}
      local function e(id)return {constructor="ActorEndpoint",value={id=id}} end
      local function p(id,w,t)return {endpoint=e(id),wire=w,type=t,type_id=nefor.semantic_type.id(t)} end
      local actors={source={id="source",input=p("source","in",string),outputs={p("source","out",string)}},dest={id="dest",input=p("dest","in",pair),outputs={}}}
      local source; local seen={}
      local topo=Topology.new({inventory={pairs=function()return pairs(actors)end},semantic=nefor.semantic_type,settle_result=function()end,
        dispatch=function(_,id,_,arrival) if id=="source" then source=arrival else seen[#seen+1]=arrival.payload.value end end})
      local function assemble(slot)return {constructor="Assemble",value={inputs={string,string},output=pair,slot=slot,path={4},kind="product"}} end
      local mod={actors={actors.source,actors.dest},messages={},nodes={},kills={},routes={
        {id="a",from=actors.source.outputs[1],to=actors.dest.input,transforms={assemble(0)}},
        {id="b",from=actors.source.outputs[1],to=actors.dest.input,transforms={assemble(1)}}}}
      local state,err=topo:preflight(mod);assert(state,err);topo:install(mod,state)
      topo:initial({to=actors.source.input,transforms={},semantic_type=string,semantic_type_id="String",content={value="same",semantic_value="same"}})
      topo:route("source","out",source)
      return seen
    "#).eval().unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!([["same", "same"]])
    );
}

#[test]
fn actor_free_result_boundary_bootstraps_without_output_actor() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local Topology=require("topology")
      local unit={kind="primitive",name="Unit"}; local string={kind="primitive",name="String"}
      local settled
      local topo=Topology.new({inventory={pairs=function()return pairs({})end},semantic=nefor.semantic_type,
        dispatch=function()error("no actor should be dispatched")end,
        settle_result=function(from,value) settled={from=from,value=value.value} end})
      topo:set_result({type={kind="list",item=string},type_id="List<String>",leaves={},through={{steps={
        {constructor="Unit",value=unit},{constructor="EmptyList",value=string}}}}})
      assert(topo:bootstrap_result())
      return settled
    "#).eval().unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!({"from":"mag.result","value":{}})
    );
}

#[test]
fn delta_template_materialization_preserves_route_and_message_transforms() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local operations=require("operations")
      local unit={kind="primitive",name="Unit"}
      local local_ref={constructor="LocalActorRef",value={slot="worker"}}
      local function p(ref,w)return {endpoint=ref,type=unit,type_id="Unit",wire=w} end
      local operation={expressions={{constructor="Trigger",value={id="trigger",result_type="String"}}},captures={},template={
        types={Unit=unit},actors={{slot="worker",id="trigger",factory="stub",type_arguments={},params={},input=p(local_ref,"in"),outputs={p(local_ref,"out")},parameter_bindings={}}},
        routes={{from=p(local_ref,"out"),to=p(local_ref,"in"),transforms={{constructor="Unit",value=unit}}}},
        messages={{to=p(local_ref,"in"),transforms={{constructor="Unit",value=unit}},semantic_type=unit,semantic_type_id="Unit",content={constructor="Static",value={kind="in"}}}},
        nodes={},actor_reference_relocations={}}}
      local delta,err=operations.materialize(operation,"worker-1");assert(delta,err);return delta
    "#).eval().unwrap();
    let value = lua.from_value::<serde_json::Value>(result).unwrap();
    assert_eq!(value["actors"][0]["id"], "worker-1");
    assert_eq!(value["routes"][0]["transforms"][0]["constructor"], "Unit");
    assert_eq!(value["messages"][0]["transforms"][0]["constructor"], "Unit");
    assert!(value.get("junctions").is_none());
}

#[test]
fn kernel_status_outputs_observe_terminal_boundaries_once() {
    let lua = harness();
    lua.load(r#"
      local Topology=require("topology")
      local Routing=require("routing")
      for _,failure in ipairs({false, true}) do
        local wire=failure and "test.Failure" or "mag.Unit"
        local descriptor={kind="primitive",name=failure and "String" or "Unit"}
        local port={endpoint={constructor="ActorEndpoint",value={id="worker"}},wire=wire,
          type=descriptor,type_id=nefor.semantic_type.id(descriptor)}
        local actor={id="worker",outputs={port}}
        local inventory={get=function()return actor end,pairs=function()return pairs({worker=actor})end}
        local settled,observed,failed=0,0,0
        local topo=Topology.new({inventory=inventory,semantic=nefor.semantic_type,
          dispatch=function()error("terminal-only output must not dispatch")end,
          settle_result=function(_,output)
            settled=settled+1
            assert(output.value==(failure and "interrupted" or nil))
          end})
        local boundary={type=descriptor,type_id=port.type_id,leaves={{port=port,steps={}}},through={}}
        local mod={actors={},routes={},messages={},result={from=boundary}}
        local state,err=topo:preflight(mod); assert(state,err); topo:install(mod,state)
        topo:set_result(boundary)
        local router=Routing.new({inventory=inventory,registry={},
          output_port=function(id,tag)return topo:output_port(id,tag)end,
          observe_output=function(id,tag,output,arrival)
            observed=observed+1
            assert(output.arrival_id==arrival.arrival_id)
            return true,topo:observe_result({constructor="ActorEndpoint",value={id=id}},tag,arrival)
          end,
          transform_route=function(id,tag,arrival)topo:route(id,tag,arrival)end,
          events=function(e)if e.kind=="mag.run_failed" then failed=failed+1 end end})
        router:apply_completion("worker",failure and {status="failed",failure=wire,value="interrupted"} or {status="ok"})
        assert(settled==1 and observed==1 and failed==0,"status output must settle exactly once")
        topo:set_result(nil)
        router:apply_completion("worker",{status="failed",failure=wire,value="unhandled"})
        assert(failed==1,"unreachable failure escalates")
      end
    "#).exec().unwrap();
}
