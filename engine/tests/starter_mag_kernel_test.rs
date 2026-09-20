//! Runs the mag-kernel Lua unit tests (`tests/lua/mag-kernel/*.lua`) in a
//! bare Lua VM. Mirrors the harness pattern in
//! `starter_loop_counter_reasoner_test.rs`: install a minimal `nefor` stub
//! (`nefor.log` as a function, matching the real mag plugin host —
//! `plugins/mag/src/kernel.rs`, `install_nefor`), point `package.path` at
//! the kernel directory so bare requires (`require("inventory")`,
//! `require("registry")`) resolve, then exec the test chunk (which
//! `error()`s on the first failed assertion).

use std::path::PathBuf;

use mlua::{Function, Lua, LuaSerdeExt, Value, Variadic};

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .to_path_buf()
}

#[test]
fn starter_mag_kernel_fold() {
    run_lua_test("tests/lua/mag-kernel/fold_test.lua");
}

#[test]
fn starter_mag_kernel_factory_contracts() {
    run_lua_test("tests/lua/mag-kernel/factory_test.lua");
}

#[test]
fn starter_mag_kernel_routing() {
    run_lua_test("tests/lua/mag-kernel/routing_test.lua");
}

#[test]
fn starter_mag_kernel_interrupt() {
    run_lua_test("tests/lua/mag-kernel/interrupt_test.lua");
}

#[test]
fn starter_mag_kernel_flow_primitives() {
    run_lua_test("tests/lua/mag-kernel/flow_test.lua");
}

#[test]
fn starter_mag_kernel_lazy_construction() {
    run_lua_test("tests/lua/mag-kernel/lazy_construct_test.lua");
}

#[test]
fn starter_mag_kernel_observability() {
    run_lua_test("tests/lua/mag-kernel/observability_test.lua");
}

#[test]
fn starter_mag_kernel_activity_events() {
    run_lua_test("tests/lua/mag-kernel/activity_test.lua");
}

#[test]
fn starter_mag_kernel_llm_factory() {
    run_lua_test("tests/lua/mag-kernel/llm_test.lua");
}

#[test]
fn starter_mag_kernel_provider_round_metadata() {
    run_lua_test("tests/lua/mag-kernel/provider_round_metadata_test.lua");
}

#[test]
fn starter_mag_kernel_structured_output_factory() {
    run_lua_test("tests/lua/mag-kernel/structured_output_test.lua");
}

#[test]
fn starter_mag_kernel_tool_primitives() {
    run_lua_test("tests/lua/mag-kernel/tools_test.lua");
}

#[test]
fn starter_mag_kernel_process_primitives() {
    run_lua_test("tests/lua/mag-kernel/process_test.lua");
}

#[test]
fn starter_mag_kernel_adapter_factory() {
    run_lua_test("tests/lua/mag-kernel/adapter_test.lua");
}

#[test]
fn starter_mag_kernel_multi_run() {
    run_lua_test("tests/lua/mag-kernel/multi_run_test.lua");
}

#[test]
fn starter_mag_kernel_validation() {
    run_lua_test("tests/lua/mag-kernel/validation_test.lua");
}

#[test]
fn starter_mag_kernel_human_gate() {
    run_lua_test("tests/lua/mag-kernel/human_test.lua");
}

#[test]
fn starter_mag_kernel_worktree_factories() {
    run_lua_test("tests/lua/mag-kernel/worktree_test.lua");
}

fn run_lua_test(rel_path: &str) {
    let lua = Lua::new();
    install_stub_nefor(&lua).expect("install nefor stub");
    set_package_path(&lua).expect("set package.path");

    let test_path = repo_root().join(rel_path);
    let src = std::fs::read_to_string(&test_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", test_path.display()));

    if let Err(e) = lua
        .load(&src)
        .set_name(test_path.display().to_string())
        .exec()
    {
        panic!("{rel_path} failed:\n{e}");
    }
}

/// Install the minimal `nefor` global the mag kernel needs at load time:
/// `nefor.log` as a function (matching the plugin host), captured as a
/// no-op, plus `nefor.json.{encode, decode}` over serde_json — the same
/// surface the plugin host installs (`plugins/mag/src/kernel.rs`,
/// `install_json`), which the llm factory uses to serialize tool-call
/// arguments for its transcript — plus `nefor.emit`, the host's bus-emit
/// queue seam, appended to a global `__emitted` array so tests loading the
/// full kernel entry (multi_run_test) can assert on the wire traffic, plus a
/// deterministic `nefor.opaque_id` matching the host binding used for provider
/// routing correlation, and the compiler-owned semantic identity/compatibility
/// operations used by typed fixture validation. Kernel modules take their
/// logger by injection.
fn install_stub_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    let log: Function = lua.create_function(|_, _: Variadic<Value>| Ok(()))?;
    nefor.set("log", log)?;

    lua.globals().set("__emitted", lua.create_table()?)?;
    let emit = lua.create_function(|lua, body: mlua::Table| {
        let emitted: mlua::Table = lua.globals().get("__emitted")?;
        let n = emitted.raw_len();
        emitted.raw_set(n + 1, body)?;
        Ok(())
    })?;
    nefor.set("emit", emit)?;

    let mut opaque_sequence = 0_u64;
    let opaque_id = lua.create_function_mut(move |_, _: ()| {
        opaque_sequence += 1;
        Ok(format!("opaque-test-id-{opaque_sequence}"))
    })?;
    nefor.set("opaque_id", opaque_id)?;

    let json = lua.create_table()?;
    let encode = lua.create_function(|lua, value: Value| {
        let v: serde_json::Value = lua.from_value(value)?;
        serde_json::to_string(&v).map_err(|e| mlua::Error::runtime(format!("json.encode: {e}")))
    })?;
    json.set("encode", encode)?;
    let decode = lua.create_function(|lua, s: String| {
        let v: serde_json::Value = serde_json::from_str(&s)
            .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
        lua.to_value(&v)
    })?;
    json.set("decode", decode)?;
    let is_null = lua.create_function(|_, value: Value| Ok(value.is_null()))?;
    json.set("is_null", is_null)?;
    let array_metatable = lua.array_metatable();
    let is_array = lua.create_function(move |_, value: Value| {
        Ok(matches!(value, Value::Table(ref table)
            if table.metatable().is_some_and(|mt| mt.to_pointer() == array_metatable.to_pointer())))
    })?;
    json.set("is_array", is_array)?;
    let array_metatable = lua.array_metatable();
    let mark_array = lua.create_function(move |_, table: mlua::Table| {
        table.set_metatable(Some(array_metatable.clone()));
        Ok(table)
    })?;
    json.set("mark_array", mark_array)?;
    nefor.set("json", json)?;

    let semantic_type = lua.create_table()?;
    let id = lua.create_function(|lua, descriptor: Value| {
        let descriptor: serde_json::Value = lua.from_value(descriptor)?;
        let descriptor = nefor_mag::json::concrete_type_from_json(&descriptor)
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        Ok(descriptor.stable_id().to_string())
    })?;
    semantic_type.set("id", id)?;
    let constructor = lua.create_function(|lua, (descriptor, name): (Value, String)| {
        let descriptor: serde_json::Value = lua.from_value(descriptor)?;
        let descriptor = nefor_mag::json::concrete_type_from_json(&descriptor)
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        let nefor_mag::types::ConcreteType::Adt { constructors, .. } = &descriptor else {
            return Err(mlua::Error::runtime(
                "constructor lookup requires an ADT owner",
            ));
        };
        let payload = constructors
            .iter()
            .find(|candidate| candidate.name == name)
            .map(|candidate| &candidate.payload)
            .ok_or_else(|| mlua::Error::runtime(format!("unknown constructor {name}")))?;
        let result = serde_json::json!({
            "id": descriptor.constructor_id(&name)
                .map_err(|error| mlua::Error::runtime(error.to_string()))?
                .as_str(),
            "payload": nefor_mag::json::concrete_type_to_json(payload)
                .map_err(|error| mlua::Error::runtime(error.to_string()))?,
            "payload_id": payload.stable_id().as_str(),
        });
        lua.to_value(&result)
    })?;
    semantic_type.set("constructor", constructor)?;
    let validate_declarations = lua.create_function(|lua, declarations: Value| {
        let declarations: serde_json::Value = lua.from_value(declarations)?;
        let declarations = declarations
            .as_object()
            .ok_or_else(|| mlua::Error::runtime("semantic declarations must be an object"))?;
        for (id, descriptor) in declarations {
            let descriptor = nefor_mag::json::concrete_type_from_json(descriptor)
                .map_err(|error| mlua::Error::runtime(error.to_string()))?;
            let actual = descriptor.stable_id();
            if actual.as_str() != id {
                return Err(mlua::Error::runtime(format!(
                    "semantic declaration key {id} does not match descriptor identity {actual}"
                )));
            }
        }
        Ok(true)
    })?;
    semantic_type.set("validate_declarations", validate_declarations)?;
    let accepts = lua.create_function(|lua, (target, source): (Value, Value)| {
        let target: serde_json::Value = lua.from_value(target)?;
        let source: serde_json::Value = lua.from_value(source)?;
        let target = nefor_mag::json::concrete_type_from_json(&target)
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        let source = nefor_mag::json::concrete_type_from_json(&source)
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        Ok(target.accepts_edge_source(&source))
    })?;
    semantic_type.set("accepts", accepts)?;
    let input_covered_by = lua.create_function(|lua, (target, sources): (Value, Value)| {
        let target: serde_json::Value = lua.from_value(target)?;
        let sources: serde_json::Value = lua.from_value(sources)?;
        let target = nefor_mag::json::concrete_type_from_json(&target)
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        let sources = sources
            .as_array()
            .ok_or_else(|| mlua::Error::runtime("semantic product sources must be a list"))?
            .iter()
            .map(nefor_mag::json::concrete_type_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| mlua::Error::runtime(error.to_string()))?;
        Ok(target.input_is_covered_by(&sources))
    })?;
    semantic_type.set("input_covered_by", input_covered_by)?;
    // Legacy Lua fixtures omit named-type bodies; topology only needs a positive
    // validation witness after semantic identity and edge compatibility pass.
    let validate_value = lua.create_function(|lua, _: (Value, Value)| {
        let result = lua.create_table()?;
        result.set("ok", true)?;
        Ok(result)
    })?;
    semantic_type.set("validate_value", validate_value)?;
    nefor.set("semantic_type", semantic_type)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

fn set_package_path(lua: &Lua) -> mlua::Result<()> {
    let kernel_dir = repo_root().join("plugins/mag/lua/mag-kernel");
    let kernel_dir = kernel_dir.display().to_string();
    let lua_root = repo_root().join("lua");
    let lua_root = lua_root.display().to_string();
    let script = format!(
        r#"
        package.path = table.concat({{
          "{kernel_dir}/?.lua",
          "{kernel_dir}/?/init.lua",
          "{lua_root}/?.lua",
          "{lua_root}/?/init.lua",
          package.path,
        }}, ";")
        "#,
        kernel_dir = kernel_dir,
        lua_root = lua_root,
    );
    lua.load(&script).exec()
}
