use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nefor-mag-cli-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).expect("create fixture");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(&path, source).expect("write fixture");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mag"))
        .args(args)
        .output()
        .expect("run mag")
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

fn compile_args<'a>(root: &'a Path, extra: &'a [&'a str]) -> Vec<&'a str> {
    let mut args = vec!["compile", "main.mag", "--source-dir"];
    args.push(root.to_str().expect("utf8 fixture path"));
    args.extend_from_slice(extra);
    args
}

#[test]
fn compiles_with_caller_supplied_module_root_and_registry() {
    let fixture = Fixture::new("success");
    let modules = fixture.root.join("modules");
    fs::create_dir_all(&modules).expect("modules");
    fixture.write(
        "modules/contracts.mag",
        "(def contracts (foreign-contracts))",
    );
    fixture.write(
        "main.mag",
        r#"(require "contracts")
(artifact "test/v1" {:contracts contracts.contracts})"#,
    );
    let registry = fixture.write(
        "registry.lua",
        r#"return {
  registry_contracts = function(array_mt)
    local empty = function() return setmetatable({}, array_mt) end
    return setmetatable({{
      identity = "example.factory.echo",
      implementation = "echo",
      params = {},
      type_scheme = { variables = empty(), inputs = {}, input_tags = empty(), outputs = empty() },
      signals = empty(),
    }}, array_mt)
  end,
}"#,
    );
    let output = run(&compile_args(
        &fixture.root,
        &[
            "--module-root",
            modules.to_str().expect("modules path"),
            "--registry",
            registry.to_str().expect("registry path"),
        ],
    ));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json_stdout(&output);
    assert_eq!(body["version"], 1);
    assert_eq!(body["ok"], true);
    assert_eq!(body["artifact"]["format"], "test/v1");
    assert_eq!(
        body["artifact"]["data"]["contracts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(body["hash"].as_str().map(str::len), Some(64));
}

#[test]
fn syntax_type_and_graph_failures_are_structured() {
    for (name, source, code, stage) in [
        ("syntax", "(artifact", "syntax_parse", "parse"),
        (
            "type",
            "(artifact \"test/v1\" {:bad (+ 1 \"x\")})",
            "type_error",
            "typecheck",
        ),
        (
            "graph",
            "(fail {:kind \"graph\" :message \"unknown factory\"})",
            "evaluation_error",
            "evaluate",
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.write("main.mag", source);
        let output = run(&compile_args(&fixture.root, &[]));
        assert!(!output.status.success(), "{name} unexpectedly succeeded");
        assert!(!output.stderr.is_empty(), "{name} needs human stderr");
        let body = json_stdout(&output);
        assert_eq!(body["version"], 1);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["stage"], stage);
        assert!(body["error"]["message"].is_string());
    }
}

#[test]
fn path_and_registry_failures_are_structured() {
    let fixture = Fixture::new("paths");
    fixture.write("main.mag", "(artifact \"test/v1\" {})");
    let missing = fixture.root.join("missing.json");
    let output = run(&compile_args(
        &fixture.root,
        &["--registry", missing.to_str().expect("missing path")],
    ));
    assert!(!output.status.success());
    let body = json_stdout(&output);
    assert_eq!(body["error"]["code"], "registry_read");
    assert_eq!(
        body["error"]["path"],
        missing.to_str().expect("missing path")
    );
}

#[test]
fn command_surface_has_no_execute_or_run_operation() {
    for forbidden in ["execute", "run"] {
        let output = run(&[forbidden]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unrecognized subcommand"), "{stderr}");
    }

    let help = run(&["--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("compile"));
    assert!(!stdout
        .lines()
        .any(|line| line.trim_start().starts_with("execute")));
    assert!(!stdout
        .lines()
        .any(|line| line.trim_start().starts_with("run")));
}
