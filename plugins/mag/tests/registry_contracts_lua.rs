use std::path::PathBuf;

use mlua::{Function, Lua};

fn kernel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel")
}

#[test]
fn registry_exposes_qualified_serializable_contracts() {
    let lua = Lua::new();
    let package: mlua::Table = lua.globals().get("package").expect("package");
    let current: String = package.get("path").expect("package.path");
    let root = kernel_dir();
    package
        .set(
            "path",
            format!("{r}/?.lua;{r}/?/init.lua;{current}", r = root.display()),
        )
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
                  signals = {},
                },
                construct = function() return {} end,
              })
              assert(decl ~= nil, err)
              assert(reg:lookup("example") == reg:lookup("nefor.factory.example"))
              local contracts = reg:contracts()
              assert(#contracts == 1)
              assert(contracts[1].identity == "nefor.factory.example")
              assert(contracts[1].implementation == "example")
              assert(contracts[1].params.count == "int")
              assert(contracts[1].type_scheme.inputs.value == "core.String")
              assert(contracts[1].type_scheme.input_tags[1] == "core.String")
              assert(contracts[1].type_scheme.outputs[1] == "core.String")
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
    let package: mlua::Table = lua.globals().get("package").unwrap();
    let current: String = package.get("path").unwrap();
    let root = kernel_dir();
    package
        .set(
            "path",
            format!("{r}/?.lua;{r}/?/init.lua;{current}", r = root.display()),
        )
        .unwrap();
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
          outputs={{type=p("String"),wire="Same"}},routes={["Same"]={"dest"}}},
        {id="dest",factory="consumer",evidence={version=2,identity="nefor.factory.consumer",
          arguments={p("Int")},input=p("Int"),output=p("Unit")},input={type=p("Int"),wire="Same"},
          outputs={{type=p("Unit"),wire="Done"}},routes={}}
      }})
      assert(not mismatched.ok)
      assert(table.concat(mismatched.errors,"; "):find("semantic endpoint types differ"))

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
      assert(not valid(p("String"),p("String"),{kind="list"}).ok)
      assert(not valid({kind="variable",name="T"},p("String"),list(p("String"))).ok)
      assert(not valid({kind="primitive",name="String",arguments={}},p("String"),list(p("String"))).ok)
      assert(not valid({kind="constructor",name="List"},p("String"),list(p("String"))).ok)
    "#).exec().unwrap();
}
