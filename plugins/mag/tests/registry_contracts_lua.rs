use std::path::PathBuf;

use mlua::{Function, Lua, LuaSerdeExt, Value};

fn install_semantic_type(lua: &Lua) {
    let nefor = lua.create_table().unwrap();
    let semantic_type = lua.create_table().unwrap();
    semantic_type
        .set(
            "id",
            lua.create_function(|lua, descriptor: Value| {
                let descriptor: serde_json::Value = lua.from_value(descriptor)?;
                let descriptor = nefor_mag::json::concrete_type_from_json(&descriptor)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                Ok(descriptor.stable_id().to_string())
            })
            .unwrap(),
        )
        .unwrap();
    semantic_type
        .set(
            "validate_declarations",
            lua.create_function(|lua, declarations: Value| {
                let declarations: serde_json::Value = lua.from_value(declarations)?;
                let declarations = declarations
                    .as_object()
                    .ok_or_else(|| mlua::Error::runtime("declarations must be an object"))?;
                for (id, descriptor) in declarations {
                    let descriptor = nefor_mag::json::concrete_type_from_json(descriptor)
                        .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                    if descriptor.stable_id().as_str() != id {
                        return Err(mlua::Error::runtime("descriptor identity mismatch"));
                    }
                }
                Ok(true)
            })
            .unwrap(),
        )
        .unwrap();
    semantic_type
        .set(
            "accepts",
            lua.create_function(|lua, (target, source): (Value, Value)| {
                let target: serde_json::Value = lua.from_value(target)?;
                let source: serde_json::Value = lua.from_value(source)?;
                let target = nefor_mag::json::concrete_type_from_json(&target)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                let source = nefor_mag::json::concrete_type_from_json(&source)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                Ok(target.accepts_edge_source(&source))
            })
            .unwrap(),
        )
        .unwrap();
    semantic_type
        .set(
            "input_covered_by",
            lua.create_function(|lua, (target, sources): (Value, Value)| {
                let target: serde_json::Value = lua.from_value(target)?;
                let sources: serde_json::Value = lua.from_value(sources)?;
                let target = nefor_mag::json::concrete_type_from_json(&target)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                let sources = sources
                    .as_array()
                    .ok_or_else(|| mlua::Error::runtime("sources must be a list"))?
                    .iter()
                    .map(nefor_mag::json::concrete_type_from_json)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                Ok(target.input_is_covered_by(&sources))
            })
            .unwrap(),
        )
        .unwrap();
    nefor.set("semantic_type", semantic_type).unwrap();
    lua.globals().set("nefor", nefor).unwrap();
}

fn kernel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel")
}

fn package_path(root: &std::path::Path, current: &str) -> String {
    let shared_lua = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lua");
    format!(
        "{0}/?.lua;{0}/?/init.lua;{1}/?.lua;{1}/?/init.lua;{current}",
        root.display(),
        shared_lua.display()
    )
}

#[test]
fn registry_exposes_qualified_serializable_contracts() {
    let lua = Lua::new();
    let package: mlua::Table = lua.globals().get("package").expect("package");
    let current: String = package.get("path").expect("package.path");
    let root = kernel_dir();
    package
        .set("path", package_path(&root, &current))
        .expect("set package.path");

    let assertions: Function = lua
        .load(
            r#"
            local Registry = require("registry")
            return function()
              local reg = Registry.new()
              local decl, err = reg:register({
                declaration = {
                  name = "example",
                  params = { count = "int" },
                  inputs = { value = "core.String" },
                  outputs = { "core.String" },
                  semantic = {
                    input = {kind="primitive",name="String"},
                    output = {kind="primitive",name="String"},
                    inputs = {{wire="core.String",type={kind="primitive",name="String"}}},
                    outputs = {{wire="core.String",type={kind="primitive",name="String"}}},
                  },
                  signals = {},
                },
                construct = function() return {} end,
              })
              assert(decl ~= nil, err)
              assert(reg:lookup("example").declaration.name == reg:lookup("nefor.factory.example").declaration.name)
              local contracts = reg:contracts()
              assert(#contracts == 1)
              assert(contracts[1].identity == "nefor.factory.example")
              assert(contracts[1].implementation == "example")
              assert(contracts[1].params.count == "int")
              assert(contracts[1].type_scheme.inputs.value == "core.String")
              assert(contracts[1].type_scheme.input_tags[1] == "core.String")
              assert(contracts[1].type_scheme.outputs[1] == "core.String")
              assert(contracts[1].type_scheme.semantic.input.name == "String")
              assert(contracts[1].type_scheme.semantic.inputs[1].wire == "core.String")
            end
            "#,
        )
        .eval()
        .expect("compile assertions");

    assertions
        .call::<()>(())
        .expect("registry contract assertions");
}

#[test]
fn registry_requires_compiler_specialization_for_generic_factories() {
    let lua = Lua::new();
    install_semantic_type(&lua);
    let package: mlua::Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = kernel_dir();
    package.set("path", package_path(&root, &current)).unwrap();
    lua.load(r#"
      local Registry = require("registry")
      local reg = Registry.new()
      local function p(name) return {kind="primitive",name=name} end
      local function v(name) return {kind="variable",name=name} end
      local function n(name,args) return {kind="named",name=name,arguments=args or {}} end
      local function list(item) return {kind="list",item=item} end
      local function map(key,value) return {kind="map",key=key,value=value} end
      local function record(fields) return {kind="record",fields=fields} end
      assert(reg:register({ declaration = {
        name="generic", type_variables={"T"}, semantic={input=v("T"),output=list(v("T")),inputs={{wire="In",type=v("T")}},outputs={{wire="Out",type=list(v("T"))}}},
        params={}, inputs={input="In"}, outputs={"Out"}
      }, construct=function() return {} end }))
      local function valid(argument,input,output)
        return reg:validate_modification({actors={{id="a",factory="generic",evidence={
          version=2,identity="nefor.factory.generic",arguments={argument},input=input,output=output},
          input={type=input,wire="In"},outputs={{type=output,wire="Out"}},routes={}}}})
      end
      local task=n("main.Task")
      local good=valid(task,task,list(task))
      assert(good.ok, table.concat(good.errors or {}, "; "))
      local structural=record({
        {name="description",type=p("String")},
        {name="metadata",type=map(p("String"),list(record({{name="value",type=p("Int")}})))},
      })
      local structural_good=valid(structural,structural,list(structural))
      assert(structural_good.ok,table.concat(structural_good.errors or {},"; "))
      assert(not reg:validate_modification({actors={{id="m",factory="nefor.factory.generic",routes={}}}}).ok)
      assert(not valid(task,p("Int"),list(task)).ok)
      assert(not valid(task,task,list(p("Int"))).ok)
      assert(not valid({kind="named",name="Task",arguments={}},task,list(task)).ok)
      assert(reg:register({ declaration = {
        name="plain", params={}, inputs={input="In"}, outputs={"Out"}
      }, construct=function() return {} end }))
      assert(reg:validate_modification({actors={{id="p",factory="plain",routes={}}}}).ok)

      assert(reg:register({declaration={name="fixed",semantic={
        input=p("String"),output=p("Int"),
        inputs={{wire="FixedIn",type=p("String")}},outputs={{wire="FixedOut",type=p("Int")}}},
        params={},inputs={value="FixedIn"},outputs={"FixedOut"}},construct=function() return {} end}))
      assert(not reg:validate_modification({actors={{id="fixed",factory="fixed",
        input={wire="FixedIn",type=p("String")},outputs={{wire="FixedOut",type=p("Int")}},routes={}}}}).ok)
      local fixed_good=reg:validate_modification({actors={{id="fixed",factory="fixed",evidence={
        version=2,identity="nefor.factory.fixed",arguments={},input=p("String"),output=p("Int")},
        input={wire="FixedIn",type=p("String")},outputs={{wire="FixedOut",type=p("Int")}},routes={}}}})
      assert(fixed_good.ok,table.concat(fixed_good.errors or {},"; "))
      local string_id=nefor.semantic_type.id(p("String"))
      local int_id=nefor.semantic_type.id(p("Int"))
      local typed_fixed=reg:validate_modification({
        types={[string_id]=p("String"),[int_id]=p("Int")},
        actors={{id="fixed",factory="fixed",evidence={
          version=2,identity="nefor.factory.fixed",arguments={},input=p("String"),output=p("Int")},
          input={wire="FixedIn",type=p("String"),type_id=string_id},
          outputs={{wire="FixedOut",type=p("Int"),type_id=int_id}},routes={}}}})
      assert(typed_fixed.ok,table.concat(typed_fixed.errors or {},"; "))
      local forged_key=reg:validate_modification({
        types={["sha256:forged"]=p("String")},
        actors={}})
      assert(not forged_key.ok)
      assert(table.concat(forged_key.errors,"; "):find("declarations are invalid"))
      local mismatched_ref=reg:validate_modification({
        types={[string_id]=p("String"),[int_id]=p("Int")},
        actors={{id="fixed",factory="fixed",evidence={
          version=2,identity="nefor.factory.fixed",arguments={},input=p("String"),output=p("Int")},
          input={wire="FixedIn",type=p("String"),type_id=int_id},
          outputs={{wire="FixedOut",type=p("Int"),type_id=int_id}},routes={}}}})
      assert(not mismatched_ref.ok)
      assert(table.concat(mismatched_ref.errors,"; "):find("absent or mismatched"))
      local fixed_tamper=reg:validate_modification({actors={{id="fixed",factory="fixed",evidence={
        version=2,identity="nefor.factory.fixed",arguments={},input=p("String"),output=p("Int")},
        input={wire="FixedIn",type=p("Int")},outputs={{wire="FixedOut",type=p("String")}},routes={}}}})
      assert(not fixed_tamper.ok)

      local function contradictory(name,inputs,outputs,semantic_inputs,semantic_outputs)
        local _,err=reg:register({declaration={name=name,params={},inputs=inputs,outputs=outputs,
          semantic={input=p("String"),output=p("String"),inputs=semantic_inputs,outputs=semantic_outputs}},
          construct=function() return {} end})
        return err
      end
      assert(contradictory("missing-wire",{value="In"},{"Out"},{{wire="In",type=p("String")}},{}):find("output wires"))
      assert(contradictory("extra-wire",{value="In"},{"Out"},{{wire="In",type=p("String")}},
        {{wire="Out",type=p("String")},{wire="Extra",type=p("String")}}):find("output wires"))
      assert(contradictory("swapped-wires",{value="In"},{"Out"},{{wire="Out",type=p("String")}},
        {{wire="In",type=p("String")}}):find("input wires"))

      assert(reg:register({declaration={name="producer",type_variables={"T"},
        semantic={input=p("Unit"),output=v("T"),inputs={{wire="Start",type=p("Unit")}},outputs={{wire="Same",type=v("T")}}},params={},inputs={start="Start"},outputs={"Same"}},
        construct=function() return {} end}))
      assert(reg:register({declaration={name="consumer",type_variables={"T"},
        semantic={input=v("T"),output=p("Unit"),inputs={{wire="Same",type=v("T")}},outputs={{wire="Done",type=p("Unit")}}},params={},inputs={value="Same"},outputs={"Done"}},
        construct=function() return {} end}))
      local mismatched=reg:validate_modification({actors={
        {id="source",factory="producer",evidence={version=2,identity="nefor.factory.producer",
          arguments={p("String")},input=p("Unit"),output=p("String")},input={type=p("Unit"),wire="Start"},
          outputs={{type=p("String"),wire="Same"}},routes={["Same"]={{actor="dest",wire="Same"}}}},
        {id="dest",factory="consumer",evidence={version=2,identity="nefor.factory.consumer",
          arguments={p("Int")},input=p("Int"),output=p("Unit")},input={type=p("Int"),wire="Same"},
          outputs={{type=p("Unit"),wire="Done"}},routes={}}
      }})
      assert(not mismatched.ok)
      assert(table.concat(mismatched.errors,"; "):find("semantic endpoint types differ"))

      assert(reg:register({declaration={name="product-consumer",type_variables={"T"},
        semantic={input={kind="product",items={v("T"),v("T")}},output=p("Unit"),
          inputs={{wire="Pair",type={kind="product",items={v("T"),v("T")}}}},
          outputs={{wire="Done",type=p("Unit")}}},params={},inputs={value={"Pair","Pair"}},outputs={"Done"}},
        construct=function() return {} end}))
      local repeated_product_actors={
        {id="left",factory="producer",evidence={version=2,identity="nefor.factory.producer",
          arguments={p("String")},input=p("Unit"),output=p("String")},input={type=p("Unit"),wire="Start"},
          outputs={{type=p("String"),wire="Same"}},routes={Same={{actor="pair",wire="Pair"}}}},
        {id="right",factory="producer",evidence={version=2,identity="nefor.factory.producer",
          arguments={p("String")},input=p("Unit"),output=p("String")},input={type=p("Unit"),wire="Start"},
          outputs={{type=p("String"),wire="Same"}},routes={Same={{actor="pair",wire="Pair"}}}},
        {id="pair",factory="product-consumer",evidence={version=2,identity="nefor.factory.product-consumer",
          arguments={p("String")},input={kind="product",items={p("String"),p("String")}},output=p("Unit")},
          input={type={kind="product",items={p("String"),p("String")}},wire="Pair"},
          outputs={{type=p("Unit"),wire="Done"}},routes={}}
      }
      local repeated_product=reg:validate_modification({actors=repeated_product_actors})
      assert(repeated_product.ok,table.concat(repeated_product.errors or {},"; "))
      local killed_source=reg:validate_modification({kills={"right"}},nil,repeated_product_actors)
      assert(not killed_source.ok)
      assert(table.concat(killed_source.errors,"; "):find("component multiset"))
      local killed_target=reg:validate_modification({kills={"pair"}},nil,repeated_product_actors)
      assert(killed_target.ok,table.concat(killed_target.errors or {},"; "))
      local tombstoned_right=repeated_product_actors[2]
      local dead_respawn=reg:validate_modification(
        {actors={tombstoned_right}},
        function(id)
          if id=="right" then return "nefor.factory.producer","dead",tombstoned_right end
          if id=="left" then return "nefor.factory.producer","alive",repeated_product_actors[1] end
          if id=="pair" then return "nefor.factory.product-consumer","alive",repeated_product_actors[3] end
          return nil
        end,
        {repeated_product_actors[1],repeated_product_actors[3]})
      assert(not dead_respawn.ok)
      assert(table.concat(dead_respawn.errors,"; "):find("component multiset"))
      local underfilled=reg:validate_modification({actors={
        repeated_product_actors[1],repeated_product_actors[3]}})
      assert(not underfilled.ok)
      assert(table.concat(underfilled.errors,"; "):find("component multiset"))
      local third={id="third",factory="producer",evidence={version=2,identity="nefor.factory.producer",
        arguments={p("String")},input=p("Unit"),output=p("String")},input={type=p("Unit"),wire="Start"},
        outputs={{type=p("String"),wire="Same"}},routes={Same={{actor="pair",wire="Pair"}}}}
      local overfilled=reg:validate_modification({actors={
        repeated_product_actors[1],repeated_product_actors[2],third,repeated_product_actors[3]}})
      assert(not overfilled.ok)
      assert(table.concat(overfilled.errors,"; "):find("component multiset"))

      assert(reg:register({declaration={name="choice",type_variables={"A","B"},
        semantic={input=p("Unit"),output={kind="union",items={v("A"),v("B")}},inputs={{wire="Start",type=p("Unit")}},outputs={{wire="Left",type=v("A")},{wire="Right",type=v("B")}}},params={},inputs={start="Start"},outputs={"Left","Right"}},
        construct=function() return {} end}))
      local missing_arm=reg:validate_modification({actors={{id="choice",factory="choice",
        evidence={version=2,identity="nefor.factory.choice",arguments={p("String"),p("Int")},
          input=p("Unit"),output={kind="union",items={p("String"),p("Int")}}},input={type=p("Unit"),wire="Start"},
        outputs={{type=p("String"),wire="Left"}},routes={}}}})
      assert(not missing_arm.ok)
      assert(table.concat(missing_arm.errors,"; "):find("semantic output for wire"))
      local swapped=reg:validate_modification({actors={{id="choice",factory="choice",
        evidence={version=2,identity="nefor.factory.choice",arguments={p("String"),p("Int")},
          input=p("Unit"),output={kind="union",items={p("String"),p("Int")}}},input={type=p("Unit"),wire="Start"},
        outputs={{type=p("Int"),wire="Left"},{type=p("String"),wire="Right"}},routes={}}}})
      assert(not swapped.ok)

      assert(reg:register({declaration={name="answer",semantic={
        input=p("String"),output=n("nefor.contracts.TextAnswer"),
        inputs={{wire="In",type=p("String")}},
        outputs={{wire="Final",type=n("nefor.contracts.TextAnswer")}}},
        params={},inputs={value="In"},outputs={"Final"}},construct=function() return {} end}))
      local refined=reg:validate_modification({actors={{id="answer",factory="answer",evidence={
        version=2,identity="nefor.factory.answer",arguments={},input=p("String"),
        output=n("nefor.contracts.TextAnswer")},input={wire="In",type=p("String")},
        outputs={{wire="Final",type=n("main.CodeAudit")}},routes={}}}})
      assert(refined.ok,table.concat(refined.errors or {},"; "))
      local structural_refinement=reg:validate_modification({actors={{id="answer",factory="answer",evidence={
        version=2,identity="nefor.factory.answer",arguments={},input=p("String"),
        output=n("nefor.contracts.TextAnswer")},input={wire="In",type=p("String")},
        outputs={{wire="Final",type=p("String")}},routes={}}}})
      assert(not structural_refinement.ok)
      assert(not valid(p("String"),p("String"),{kind="list"}).ok)
      assert(not valid({kind="variable",name="T"},p("String"),list(p("String"))).ok)
      assert(not valid({kind="primitive",name="String",arguments={}},p("String"),list(p("String"))).ok)
      assert(not valid({kind="constructor",name="List"},p("String"),list(p("String"))).ok)
    "#).exec().unwrap();
}
