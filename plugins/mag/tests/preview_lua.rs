use std::path::PathBuf;

use mlua::{Function, Lua, LuaSerdeExt, Value};

fn kernel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lua/mag-kernel")
}

fn lua_with_kernel_path() -> Lua {
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
    lua
}

#[test]
fn preview_dsl_is_plain_serializable_data_and_validates_bindings() {
    let lua = lua_with_kernel_path();
    let preview: mlua::Table = lua.load("return require('preview')").eval().unwrap();
    let declaration: mlua::Table = lua
        .load("return { params={command='string'}, inputs={input='In'}, outputs={'Out'} }")
        .eval()
        .unwrap();
    let description: Value = lua
        .load(
            r#"local p=require('preview'); return p.column { children = {
              p.text { value=p.param('command') },
              p.stream { source=p.stream_ref('events',{kind='record',fields={text='string'}}), item=p.text { value=p.item('text') } },
              p.value { value=p.state('exit','table?') }
            }}"#,
        )
        .eval()
        .unwrap();
    let validate: Function = preview.get("validate").unwrap();
    let validated: mlua::Table = validate
        .call((description.clone(), declaration.clone()))
        .unwrap();
    let json: serde_json::Value = lua.from_value(description).unwrap();
    assert_eq!(json["kind"], "column");
    let bindings: mlua::Table = validated.get("bindings").unwrap();
    assert_eq!(
        bindings
            .get::<mlua::Table>("events")
            .unwrap()
            .get::<String>("kind")
            .unwrap(),
        "stream"
    );
    for (name, kind) in [("command", "param"), ("input", "input"), ("Out", "output")] {
        let description: Value = lua
            .load(format!(
                "local p=require('preview'); return p.value {{ value=p.{kind}('{name}') }}"
            ))
            .eval()
            .unwrap();
        let validated: mlua::Table = validate.call((description, declaration.clone())).unwrap();
        let declared: mlua::Table = validated
            .get::<mlua::Table>("bindings")
            .unwrap()
            .get(name)
            .unwrap();
        assert_eq!(declared.get::<String>("kind").unwrap(), kind);
    }

    let bad: Value = lua
        .load("local p=require('preview'); return p.text { value=p.param('missing') }")
        .eval()
        .unwrap();
    let result = validate.call::<mlua::MultiValue>((bad, declaration));
    let values = result.unwrap();
    assert!(matches!(values.front(), Some(Value::Nil)));
    assert!(values
        .get(1)
        .and_then(Value::as_string)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("unknown param"));
}

#[test]
fn preview_validation_rejects_recursive_and_schema_mismatches() {
    let lua = lua_with_kernel_path();
    lua.load(
        r#"
      local p=require('preview')
      local declaration={params={},inputs={},outputs={}}
      local function rejects(description, needle)
        local _,err=p.validate(description,declaration)
        assert(err and err:find(needle,1,true), tostring(err))
      end

      local cycle=p.column {children={}}
      cycle.children[1]=cycle
      rejects(cycle,'cycle')
      rejects(p.text {value=function() end},'not serializable')
      rejects({kind='text',value='ok',surprise=true},'unknown field')
      rejects({kind='column',children={[1]=p.text{value='a'},[3]=p.text{value='b'}}},'dense list')

      local item={kind='variant',tag='kind',cases={
        line={kind='record',fields={text='string'}},
        code={kind='record',fields={value='number'}},
      }}
      rejects(p.stream {source=p.stream_ref('events',item),item=p.cases {
        line=p.text {value=p.item('text')},
        extra=p.text {value='x'},
      }},'unknown case tag')
      rejects(p.stream {source=p.stream_ref('events',item),item=p.cases {
        line=p.text {value=p.item('missing')},
        code=p.text {value='x'},
      }},'unknown item field')
      rejects(p.stream {source=p.stream_ref('events',item),item=p.cases {
        line=p.text {value=p.item('text')},
      }},'missing case tag')
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn every_shipped_factory_declares_a_valid_preview() {
    let lua = lua_with_kernel_path();
    lua.load(
        r#"
        local preview=require('preview')
        local factories={
          'stub','sink','source','output','human','llm','structured-output',
          'collector','collect-item','collected-prompt','run-tool','tool-result',
          'adapter','bash','worktree-create','worktree-open',
        }
        for _,name in ipairs(factories) do
          local factory=require('factories.'..name)
          assert(type(factory.declaration.preview)=='table', name..' preview missing')
          local validated,err=preview.validate(factory.declaration.preview,factory.declaration)
          assert(validated, name..': '..tostring(err))
        end
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn llm_and_structured_output_share_the_transcript_presentation() {
    let lua = lua_with_kernel_path();
    lua.load(
        r#"
        local llm=require('factories.llm').declaration.preview
        local structured=require('factories.structured-output').declaration.preview
        local transcript=structured.children[1]
        assert(transcript.kind=='stream')
        local function same(a,b)
          if type(a)~=type(b) then return false end
          if type(a)~='table' then return a==b end
          for k,v in pairs(a) do if not same(v,b[k]) then return false end end
          for k in pairs(b) do if a[k]==nil then return false end end
          return true
        end
        assert(same(llm,transcript),'structured-output transcript drifted from llm')
        assert(llm.item.values.reasoning.format=='reasoning')
        assert(llm.item.values.reasoning.style=='reasoning')
        assert(llm.item.values.tool_call.format=='tool_call')
        assert(llm.item.values.tool_result.format=='tool_result')
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn mandatory_preview_rejects_missing_and_registry_contract_advertises_description() {
    let lua = lua_with_kernel_path();
    lua.load(
        r#"
        local Registry=require('registry')
        local reg=Registry.new({require_preview=true})
        local _,missing=reg:register({declaration={name='missing',params={},inputs={},outputs={},signals={}},construct=function()end})
        assert(missing:find('preview is required'))
        local authored={name='ok',params={},inputs={},outputs={},signals={},preview={kind='text',value='ready'}}
        local decl,err=reg:register({declaration=authored,construct=function()end})
        assert(decl,err)
        authored.preview.value='mutated'
        local contracts=reg:contracts()
        assert(contracts[1].preview.kind=='text')
        assert(contracts[1].preview.value=='ready')
        contracts[1].preview.value='snapshot mutation'
        assert(reg:contracts()[1].preview.value=='ready')
        local looked_up=reg:lookup('ok')
        looked_up.declaration.preview.value='lookup mutation'
        assert(reg:contracts()[1].preview.value=='ready')
        "#,
    )
    .exec()
    .unwrap();
}
