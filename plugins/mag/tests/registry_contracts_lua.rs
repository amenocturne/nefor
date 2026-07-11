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
