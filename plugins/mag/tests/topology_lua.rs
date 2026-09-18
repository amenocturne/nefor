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
    lua.load(
        r#"
        local function type_id(value)
          if value.kind == "primitive" then return value.name end
          if value.kind == "list" then return "List<" .. type_id(value.item) .. ">" end
          if value.kind == "product" then
            local ids = {}; for i,item in ipairs(value.items) do ids[i]=type_id(item) end
            return "(" .. table.concat(ids, ",") .. ")"
          end
          return value.name
        end
        local function validate(descriptor, value)
          if descriptor.kind == "primitive" then
            if descriptor.name == "Unit" then return value == nil end
            if descriptor.name == "String" then return type(value) == "string" end
            if descriptor.name == "Int" then return type(value) == "number" and value % 1 == 0 end
            if descriptor.name == "Bool" then return type(value) == "boolean" end
            return true
          elseif descriptor.kind == "list" then
            if type(value) ~= "table" then return false end
            for _,item in ipairs(value) do if not validate(descriptor.item,item) then return false end end
            return true
          elseif descriptor.kind == "product" then
            if type(value) ~= "table" or #value ~= #descriptor.items then return false end
            for i,item in ipairs(descriptor.items) do if not validate(item,value[i]) then return false end end
            return true
          elseif descriptor.kind == "adt" then
            if type(value) ~= "table" or type(value.constructor) ~= "string" or value.value == nil then return false end
            for _,candidate in ipairs(descriptor.constructors) do
              if candidate.name == value.constructor then return validate(candidate.payload,value.value) end
            end
            return false
          end
          return true
        end
        nefor = {
          json = { encode = function(_) return "encoded" end,
                   mark_array = function(value) return value end },
          semantic_type = {
            id = type_id,
            accepts = function(target, source) return type_id(target) == type_id(source) end,
            validate_value = function(descriptor, value) return {ok=validate(descriptor,value)} end,
            constructor = function(owner, name)
              for _,candidate in ipairs(owner.constructors or {}) do
                if candidate.name == name then return {
                  id=owner.name .. "." .. name,
                  payload=candidate.payload,
                  payload_id=type_id(candidate.payload),
                } end
              end
              error("unknown constructor")
            end,
          },
        }
        "#,
    )
    .exec()
    .unwrap();
    lua
}

#[test]
fn junction_routes_drain_nonrecursively_in_route_order() {
    let lua = harness();
    let result: Value = lua
        .load(
            r#"
            local Topology=require("topology")
            local seen={}
            local topology
            topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,
              dispatch=function(kind,id,port,arrival) assert(kind=="junction"); topology:enqueue(id,port,arrival) end,
              observe=function(id,wire,arrival,payload) seen[#seen+1]={id=id,wire=wire,value=payload.value}; return true end})
            local string={kind="primitive",name="String"}
            local function endpoint(id) return {constructor="JunctionEndpoint",value={id=id}} end
            local function port(id,wire) return {endpoint=endpoint(id),type=string,type_id="String",wire=wire} end
            local a={id="a",operation={constructor="Pass"},inputs={port("a","in")},outputs={port("a","out")}}
            local b={id="b",operation={constructor="Pass"},inputs={port("b","in")},outputs={port("b","out")}}
            local route={id="a-b",from=port("a","out"),to=port("b","in"),product_position=-1}
            local mod={actors={},junctions={a,b},routes={route},messages={{to=port("a","in"),semantic_type=string,semantic_type_id="String",content={kind="in",value="ok"}}},nodes={},kills={}}
            local state,err=topology:preflight(mod); assert(state,err); topology:install(mod,state)
            topology:initial(mod.messages[1]); assert(topology:drain())
            return seen
            "#,
        )
        .eval()
        .unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!([
            {"id":"a","wire":"out","value":"ok"},
            {"id":"b","wire":"out","value":"ok"}
        ])
    );
}

#[test]
fn joins_form_fifo_cohorts_by_distinct_slots_with_shared_and_alternative_producers() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local Topology=require("topology")
      local outputs={}
      local topology
      topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,
        dispatch=function(kind,id,port,arrival) topology:enqueue(id,port,arrival) end,
        observe=function(id,wire,arrival,payload)
          if id=="join" then outputs[#outputs+1]=payload.value end
          return true
        end})
      local string={kind="primitive",name="String"}
      local pair={kind="product",items={string,string}}
      local function e(id) return {constructor="JunctionEndpoint",value={id=id}} end
      local function p(id,wire,t,tid) return {endpoint=e(id),type=t or string,type_id=tid or "String",wire=wire} end
      local function pass(id) return {id=id,operation={constructor="Pass"},inputs={p(id,"in")},outputs={p(id,"out")}} end
      local join={id="join",operation={constructor="ProductJoin"},
        inputs={p("join","slot:0"),p("join","slot:1")},outputs={p("join","out",pair,"(String,String)")}}
      local junctions={pass("shared"),pass("alternate"),pass("right"),join}
      local routes={
        {id="shared-left",from=p("shared","out"),to=p("join","slot:0"),product_position=-1},
        {id="shared-right",from=p("shared","out"),to=p("join","slot:1"),product_position=-1},
        {id="alternate-left",from=p("alternate","out"),to=p("join","slot:0"),product_position=-1},
        {id="right-right",from=p("right","out"),to=p("join","slot:1"),product_position=-1},
      }
      local mod={actors={},junctions=junctions,routes=routes,messages={},nodes={},kills={}}
      local state,err=topology:preflight(mod); assert(state,err); topology:install(mod,state)
      local function send(id,value)
        local message={to=p(id,"in"),semantic_type=string,semantic_type_id="String",content={value=value}}
        topology:initial(message)
      end
      -- One producer fans out to two equal-typed, distinct slots.
      send("shared","S1"); send("shared","S2"); assert(topology:drain())
      -- Alternative producers feed each slot; declaration order differs from arrival order.
      send("right","R1"); send("alternate","L1");
      send("alternate","L2"); send("right","R2"); assert(topology:drain())
      return outputs
    "#).eval().unwrap();
    assert_eq!(
        lua.from_value::<serde_json::Value>(result).unwrap(),
        json!([["S1", "S1"], ["S2", "S2"], ["L1", "R1"], ["L2", "R2"]])
    );
}

#[test]
fn unit_replaces_the_old_payload_while_pass_preserves_false() {
    harness().load(r#"
      local Topology=require("topology")
      local seen={}
      local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,
        dispatch=function() end,observe=function(id,wire,arrival,payload) seen[id]=payload; return true end})
      local bool={kind="primitive",name="Bool"}; local unit={kind="primitive",name="Unit"}
      local function e(id) return {constructor="JunctionEndpoint",value={id=id}} end
      local function p(id,w,t,tid) return {endpoint=e(id),type=t,type_id=tid,wire=w} end
      local pass={id="pass",operation={constructor="Pass"},inputs={p("pass","in",bool,"Bool")},outputs={p("pass","out",bool,"Bool")}}
      local discard={id="discard",operation={constructor="Unit"},inputs={p("discard","in",bool,"Bool")},outputs={p("discard","out",unit,"Unit")}}
      local mod={actors={},junctions={pass,discard},routes={},messages={},nodes={},kills={}}
      local state,err=topology:preflight(mod); assert(state,err); topology:install(mod,state)
      topology:initial({to=pass.inputs[1],semantic_type=bool,semantic_type_id="Bool",content={value=false,semantic_value=false,calls={1}}})
      topology:initial({to=discard.inputs[1],semantic_type=bool,semantic_type_id="Bool",content={value=false,semantic_value=false,calls={1}}})
      assert(topology:drain())
      assert(seen.pass.value == false and seen.pass.semantic_value == false and seen.pass.calls[1] == 1)
      assert(seen.discard.value == nil and seen.discard.semantic_value == nil and seen.discard.calls == nil)
    "#).exec().unwrap();
}

#[test]
fn adt_operations_validate_specs_and_runtime_wrappers() {
    harness().load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}; local integer={kind="primitive",name="Int"}
      local result={kind="adt",name="test.Result",arguments={},constructors={{name="Error",payload=string},{name="Ok",payload=integer}}}
      local function e(id) return {constructor="JunctionEndpoint",value={id=id}} end
      local function p(id,w,t,tid) return {endpoint=e(id),type=t,type_id=tid,wire=w} end
      local unpack={id="unpack",operation={constructor="AdtUnpack",value={owner=result,branches={
        {constructor="Error",payload=string,wire="error"},{constructor="Ok",payload=integer,wire="ok"}}}},
        inputs={p("unpack","in",result,"test.Result")},outputs={p("unpack","error",string,"String"),p("unpack","ok",integer,"Int")}}
      local seen={}; local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,
        dispatch=function() end,observe=function(id,wire,arrival,payload) seen[#seen+1]={wire=wire,value=payload.value,semantic=payload.semantic_value,constructor=arrival.constructor_id}; return true end})
      local mod={actors={},junctions={unpack},routes={},messages={},nodes={},kills={}}
      local state,err=topology:preflight(mod); assert(state,err); topology:install(mod,state)
      topology:initial({to=unpack.inputs[1],semantic_type=result,semantic_type_id="test.Result",content={value={constructor="Ok",value=7},semantic_value={constructor="Ok",value=7}}})
      assert(topology:drain()); assert(seen[1].wire=="ok" and seen[1].value==7 and seen[1].semantic==7 and seen[1].constructor=="Int")
      topology:initial({to=unpack.inputs[1],semantic_type=result,semantic_type_id="test.Result",content={value={constructor="Ok",value=8},semantic_value={constructor="Error",value="wrong"}}})
      local ok,error=topology:drain(); assert(not ok and error:find("semantic wrapper"))
      topology:initial({to=unpack.inputs[1],semantic_type=result,semantic_type_id="test.Result",content={value={constructor="Ok",value="wrong"},semantic_value={constructor="Ok",value=8}}})
      ok,error=topology:drain(); assert(not ok and error:find("raw value"),tostring(error))

      local malformed={id="bad",operation={constructor="AdtPack",value={owner=result,payload=string,constructor="Ok"}},
        inputs={p("bad","in",string,"String")},outputs={p("bad","out",result,"test.Result")}}
      local rejected,why=topology:preflight({actors={},junctions={malformed},routes={},messages={},nodes={},kills={}})
      assert(not rejected and why:find("invalid AdtPack signature"))
    "#).exec().unwrap();
}

#[test]
fn actor_and_junction_ids_use_separate_namespaces() {
    harness().load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}
      local function endpoint(kind,id) return {constructor=kind,value={id=id}} end
      local function p(kind,id,w) return {endpoint=endpoint(kind,id),type=string,type_id="String",wire=w} end
      local actor={id="same",factory="capability",type_arguments={},input=p("ActorEndpoint","same","in"),outputs={p("ActorEndpoint","same","out")},params={}}
      local junction={id="same",operation={constructor="Pass"},inputs={p("JunctionEndpoint","same","in")},outputs={p("JunctionEndpoint","same","out")}}
      local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,dispatch=function() end,observe=function() end})
      local state,err=topology:preflight({actors={actor},junctions={junction},routes={},messages={},nodes={{members={"same"}}},kills={}})
      assert(state,err); assert(state.actors.same==actor and state.junctions.same==junction)
    "#).exec().unwrap();
}

#[test]
fn preflight_rejects_malformed_topology_without_installing_anything() {
    harness().load(r#"
      local Topology=require("topology")
      local string={kind="primitive",name="String"}; local unit={kind="primitive",name="Unit"}
      local function e(id) return {constructor="JunctionEndpoint",value={id=id}} end
      local function p(id,w,t,tid) return {endpoint=e(id),type=t or string,type_id=tid or "String",wire=w} end
      local function pass(id) return {id=id,operation={constructor="Pass"},inputs={p(id,"in")},outputs={p(id,"out")}} end
      local function reject(mod,pattern)
        local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,dispatch=function() end,observe=function() end})
        local state,err=topology:preflight(mod); assert(not state and err:find(pattern),tostring(err))
        assert(next(topology.junctions)==nil and #topology.routes==0)
      end
      local base=function(junctions,routes) return {actors={},junctions=junctions or {},routes=routes or {},messages={},nodes={},kills={}} end
      reject(base({{id="bad",operation={constructor="Unknown"},inputs={p("bad","in")},outputs={p("bad","out")}}}),"unknown operation")
      reject(base({{id="bad",operation={constructor="Pass"},inputs={p("other","in")},outputs={p("bad","out")}}}),"does not belong")
      reject(base({{id="bad",operation={constructor="Unit"},inputs={p("bad","in")},outputs={p("bad","out")}}}),"invalid Unit signature")
      local a,b=pass("a"),pass("b")
      reject(base({a,b},{{id="bad-position",from=p("a","out"),to=p("b","in"),product_position=0}}),"product_position %-1")
      local spoof=p("a","out"); spoof.type_id="Unit"; spoof.type=unit
      reject(base({a,b},{{id="spoof",from=spoof,to=p("b","in"),product_position=-1}}),"source port is not declared")
      local kills=base({a}); kills.kills={"a"}; reject(kills,"not a live actor")
      local members=base({a}); members.nodes={{members={"a"}}}; reject(members,"not an actor")
      local pair={kind="product",items={string,string}}
      local split={id="split",operation={constructor="ProductSplit"},inputs={p("split","in",pair,"(String,String)")},outputs={p("split","left"),p("split","right")}}
      local divergent=base({split}); divergent.messages={{to=split.inputs[1],semantic_type=pair,semantic_type_id="(String,String)",content={value="raw",semantic_value={"left","right"}}}}
      reject(divergent,"malformed raw value")
    "#).exec().unwrap();
}

#[test]
fn prospective_preflight_rejects_junction_cycles_and_conflicting_duplicate_routes() {
    harness().load(r#"
      local Topology=require("topology")
      local topology=Topology.new({inventory={pairs=function() return pairs({}) end},semantic=nefor.semantic_type,dispatch=function() end,observe=function() end})
      local string={kind="primitive",name="String"}
      local function e(id) return {constructor="JunctionEndpoint",value={id=id}} end
      local function p(id,w) return {endpoint=e(id),type=string,type_id="String",wire=w} end
      local function j(id) return {id=id,operation={constructor="Pass"},inputs={p(id,"in")},outputs={p(id,"out")}} end
      local cycle={actors={},junctions={j("a"),j("b")},routes={{id="ab",from=p("a","out"),to=p("b","in"),product_position=-1},{id="ba",from=p("b","out"),to=p("a","in"),product_position=-1}},messages={},nodes={},kills={}}
      local state,err=topology:preflight(cycle); assert(not state and err:find("cycle")); assert(next(topology.junctions)==nil)
      local duplicate={actors={},junctions={j("a"),j("b")},routes={{id="same",from=p("a","out"),to=p("b","in"),product_position=-1},{id="same",from=p("b","out"),to=p("a","in"),product_position=-1}},messages={},nodes={},kills={}}
      state,err=topology:preflight(duplicate); assert(not state and err:find("conflicting route"),tostring(err))
    "#).exec().unwrap();
}

#[test]
fn delta_template_materialization_preserves_junction_endpoints_and_routes() {
    let lua = harness();
    let result: Value = lua.load(r#"
      local operations=require("operations")
      local unit={kind="primitive",name="Unit"}
      local local_ref={constructor="LocalJunctionRef",value={slot="relay"}}
      local existing_ref={constructor="ExistingJunctionRef",value={id="existing"}}
      local function port(endpoint,wire) return {endpoint=endpoint,type=unit,type_id="Unit",wire=wire} end
      local operation={
        expressions={{constructor="Trigger",value={id="trigger",result_type="String"}}}, captures={},
        template={types={Unit=unit},actors={},junctions={{slot="relay",id="trigger",
          operation={constructor="Pass"},inputs={port(local_ref,"in")},outputs={port(local_ref,"out")}}},
          routes={{from=port(local_ref,"out"),to=port(existing_ref,"in"),product_position=-1}},
          messages={},nodes={},actor_reference_relocations={}}
      }
      local delta,err=operations.materialize(operation,"relay-1"); assert(delta,err); return delta
    "#).eval().unwrap();
    let value = lua.from_value::<serde_json::Value>(result).unwrap();
    assert_eq!(value["junctions"][0]["id"], "relay-1");
    assert_eq!(
        value["routes"][0]["from"]["endpoint"]["constructor"],
        "JunctionEndpoint"
    );
    assert_eq!(
        value["routes"][0]["to"]["endpoint"]["value"]["id"],
        "existing"
    );
}

#[test]
fn opaque_endpoint_names_cannot_alias_port_addresses() {
    let lua = harness();
    lua.load(r#"
      local topology=require("topology")
      local function p(id,wire) return {endpoint={constructor="JunctionEndpoint",value={id=id}},wire=wire} end
      assert(topology.address(p("a:b","c")) ~= topology.address(p("a","b:c")))
    "#).exec().unwrap();
}
