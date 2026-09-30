#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn compile_module(destination: &Path) {
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let mut command = Command::new(compiler);
    command.arg("-shared").arg("-fPIC");
    if cfg!(target_os = "macos") {
        command.args(["-undefined", "dynamic_lookup"]);
    }
    let output = command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_module.c"))
        .arg("-o")
        .arg(destination)
        .output()
        .expect("compile independent native module");
    assert!(
        output.status.success(),
        "native fixture compilation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn composition_loads_independent_native_module_through_standard_lua_loaders() {
    let root = tempfile::tempdir().expect("temporary composition");
    let config = root.path().join("config");
    let modules = root.path().join("composition-selected-modules");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&modules).unwrap();
    let library = modules.join("external_fixture.so");
    compile_module(&library);
    std::fs::write(
        config.join("init.lua"),
        r#"
            assert(type(debug) == "table" and type(debug.getinfo) == "function")
            assert(_VERSION == "Lua 5.4")
            local root = assert(os.getenv("NATIVE_FIXTURE_ROOT"))
            package.cpath = root .. "/?.so"
            package.path = ""
            local module = require("external_fixture")
            assert(module.value == "independent native module")
            assert(require("external_fixture") == module)
            local open = assert(package.loadlib(root .. "/external_fixture.so", "luaopen_external_fixture"))
            assert(open().value == module.value)
            local missing, reason = package.loadlib(root .. "/absent.so", "luaopen_absent")
            assert(missing == nil and type(reason) == "string")
            function dispatch(_) end
            print("native module contract passed")
        "#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nefor"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(root.path().join("data"))
        .arg("plugin")
        .env("NATIVE_FIXTURE_ROOT", &modules)
        .env_remove("NEFOR_LOG_STDERR")
        .env_remove("NEFOR_LOG_FILE")
        .output()
        .expect("run engine with independent composition");
    assert!(
        output.status.success(),
        "native module composition failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("native module contract passed"));
}
