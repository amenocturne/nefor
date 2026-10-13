#![cfg(unix)]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repository root")
        .to_path_buf()
}

fn binaries() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root().join(path)
            }
        })
        .unwrap_or_else(|| root().join("target"))
        .join("debug")
}

fn lua_string(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn write_config(
    directory: &Path,
    endpoint: &str,
    result_path: &Path,
    trace_path: &Path,
    gate_binary: &Path,
    provider_binary: &Path,
) {
    let repository = root();
    let source = format!(
        r#"
local ROOT = "{root}"
package.path = table.concat({{
  ROOT .. "/lua/?.lua", ROOT .. "/lua/?/init.lua",
  ROOT .. "/plugins/openai-provider/lua/?.lua",
  ROOT .. "/plugins/openai-provider/lua/?/init.lua",
  ROOT .. "/plugins/chatgpt-provider/lua/?.lua",
  ROOT .. "/plugins/chatgpt-provider/lua/?/init.lua",
  ROOT .. "/plugins/tool-gate/lua/?.lua",
  ROOT .. "/plugins/tool-gate/lua/?/init.lua",
  package.path,
}}, ";")

local ncp = require("core.ncp")
local actor = require("core.actor")
function dispatch(current_log) ncp.dispatch(current_log) end
function invoke_from_plugin(source, payload) ncp.invoke_from_plugin(source, payload) end

actor.install()
local provider = require("libs.compositors.provider")
local conversations = {{
  context = function()
    return {{ messages = {{}}, tail_messages = {{}}, watermark = 0 }}
  end,
}}
local agentic_loop = {{ config = function() return {{ provider = "chatgpt" }} end }}
actor.spawn(provider.spawn_spec("chatgpt", {{
  "{provider_binary}", "--name", "chatgpt", "--base-url", "{endpoint}",
}}, {{
  translator_lib = "chatgpt-provider",
  tool_gate = "tool-gate",
  static_token = "synthetic-test-token",
  agentic_loop = agentic_loop,
  conversations = conversations,
}}))
actor.spawn(actor.identity_spec("tool-gate", {{
  "{gate_binary}",
}}))

local OUTER_ID = "r1/capability-outer"
local state = {{ advertised = false, connected = false, invoked = false, inner_id = nil }}
local function emit(body)
  nefor.engine.send(nefor.json.encode({{
    type = "event", from = "test-driver", ts = nefor.engine.now(), body = body,
  }}))
end
local function maybe_invoke()
  if state.advertised and state.connected and not state.invoked then
    state.invoked = true
    emit({{
      kind = "tool-gate.tool.invoke",
      id = OUTER_ID,
      from = "worker.run-tool",
      name = "web_screenshot",
      args = {{ url = "https://www.iana.org/about/informational-booklet.pdf", page = 0 }},
      allowlist = {{ "web_screenshot" }},
      approval_policy = "ReadOnly",
      model = "gpt-test",
      invocation = {{
        provider = "chatgpt", model = "gpt-test", session_id = "session-1",
        run_id = "run-1", run_scope = "r1", actor_id = "worker.run-tool",
        capability_id = OUTER_ID, principal = "lead",
        conversation_id = "conversation-stable", root_conversation_id = "root",
      }},
    }})
  end
end

actor.spawn({{
  name = "test-driver",
  receive_msg = function(entry)
    local ok, envelope = pcall(nefor.json.decode, entry.payload)
    local body = ok and type(envelope) == "table" and envelope.body or nil
    if type(body) ~= "table" then return end
    local trace = assert(io.open("{trace_path}", "a"))
    trace:write(tostring(entry.origin), " ", tostring(body.kind), " ", tostring(body.state), " ", tostring(body.message), "\n")
    trace:close()
    if body.kind == "tool.register" and type(body.tools) == "table" then
      for _, tool in ipairs(body.tools) do
        if tool.name == "web_screenshot" and tool.owner == "chatgpt" then
          state.advertised = true
        end
      end
      maybe_invoke()
    elseif body.kind == "chat.auth.status" and body.provider == "chatgpt"
        and body.state == "connected" then
      state.connected = true
      maybe_invoke()
    elseif body.kind == "chatgpt.tool.invoke" and body.caller_id == OUTER_ID then
      state.inner_id = body.id
    elseif body.kind == "tool.result" and body.id == OUTER_ID then
      local artifact = {{
        advertised = state.advertised,
        inner_id = state.inner_id,
        result = body,
      }}
      local file = assert(io.open("{result_path}", "w"))
      file:write(nefor.json.encode(artifact))
      file:close()
      nefor.engine.shutdown {{ code = 0, reason = "routed web e2e complete", grace_ms = 1000 }}
    end
  end,
}})

"#,
        root = lua_string(&repository),
        provider_binary = lua_string(provider_binary),
        gate_binary = lua_string(gate_binary),
        endpoint = endpoint.replace('"', "\\\""),
        result_path = lua_string(result_path),
        trace_path = lua_string(trace_path),
    );
    fs::write(directory.join("init.lua"), source).expect("write test config");
}

fn run_engine(config: &Path, data: &Path, home: &Path, engine: &Path, trace: &Path) -> Output {
    let mut command = Command::new(engine);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("NEFOR_LOG_STDERR", "1")
        .env("RUST_LOG", "warn")
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child = command.spawn().expect("spawn nefor engine");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(output) => output.expect("wait for engine"),
        Err(_) => {
            let _ = Command::new("/bin/kill")
                .env_clear()
                .args(["-KILL", "--", &format!("-{pid}")])
                .status();
            let output = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("collect killed engine output")
                .expect("wait for killed engine");
            panic!(
                "routed web engine E2E timed out\nstdout:\n{}\nstderr:\n{}\ntrace:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                fs::read_to_string(trace).unwrap_or_else(|error| format!("<unavailable: {error}>"))
            );
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[test]
fn advertisement_gate_lua_dispatcher_client_and_outer_result_run_cross_process() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = directory.path().join("config");
    let data = directory.path().join("data");
    let home = directory.path().join("home");
    fs::create_dir_all(&config).expect("config directory");
    fs::create_dir_all(&data).expect("data directory");
    fs::create_dir_all(&home).expect("home directory");

    let engine = binaries().join("nefor");
    let gate = binaries().join("tool-gate");
    let provider = binaries().join("chatgpt-provider");
    for binary in [&engine, &gate, &provider] {
        assert!(
            binary.is_file(),
            "missing {}; run `just test-routed-web-e2e`",
            binary.display()
        );
    }

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock endpoint");
    let address = server.server_addr().to_ip().expect("server address");
    let endpoint = format!("http://{address}");
    let (request_tx, request_rx) = mpsc::channel();
    let server_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "mock endpoint did not receive /alpha/search"
            );
            let Some(mut request) = server
                .recv_timeout(remaining.min(Duration::from_millis(500)))
                .expect("receive mock request")
            else {
                continue;
            };
            if request.url().starts_with("/models?") {
                request
                    .respond(
                        tiny_http::Response::from_string(r#"{"models":[]}"#)
                            .with_status_code(200)
                            .with_header(
                                "content-type: application/json"
                                    .parse::<tiny_http::Header>()
                                    .expect("content type"),
                            ),
                    )
                    .expect("respond to models");
                continue;
            }
            if request.url() == "/alpha/search" {
                let mut raw_body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut raw_body)
                    .expect("read search request");
                let captured = (
                    request
                        .headers()
                        .iter()
                        .map(|header| (header.field.to_string(), header.value.to_string()))
                        .collect::<Vec<_>>(),
                    serde_json::from_str::<Value>(&raw_body).expect("search request JSON"),
                );
                request_tx.send(captured).expect("capture search request");
                request
                    .respond(
                        tiny_http::Response::from_string(include_str!(
                            "../../plugins/chatgpt-provider/tests/fixtures/web/screenshot-direct-url-failure.json"
                        ))
                        .with_status_code(200)
                        .with_header(
                            "content-type: application/json"
                                .parse::<tiny_http::Header>()
                                .expect("content type"),
                        ),
                    )
                    .expect("respond to search");
                break;
            }
            request
                .respond(tiny_http::Response::from_string("not found").with_status_code(404))
                .expect("respond to unexpected request");
        }
    });

    let result_path = directory.path().join("result.json");
    let trace_path = directory.path().join("trace.log");
    write_config(
        &config,
        &endpoint,
        &result_path,
        &trace_path,
        &gate,
        &provider,
    );
    let output = run_engine(&config, &data, &home, &engine, &trace_path);
    server_thread.join().expect("mock endpoint thread");
    assert!(
        output.status.success(),
        "engine failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let (headers, request) = request_rx.recv().expect("captured search request");
    assert_eq!(request["model"], "gpt-test");
    assert_eq!(
        request.pointer("/commands/screenshot/0/ref_id"),
        Some(&Value::String(
            "https://www.iana.org/about/informational-booklet.pdf".into()
        ))
    );
    assert_eq!(
        request.pointer("/commands/screenshot/0/pageno"),
        Some(&json!(0))
    );
    assert_eq!(request["settings"]["allowed_callers"], json!(["direct"]));
    assert_eq!(request["settings"]["external_web_access"], true);
    assert!(request["id"]
        .as_str()
        .is_some_and(|id| !id.is_empty() && id != "r1/capability-outer" && id != "gate-1"));
    assert_eq!(
        header(&headers, "authorization"),
        Some("Bearer synthetic-test-token")
    );
    assert_eq!(header(&headers, "session-id"), None);
    assert_eq!(header(&headers, "thread-id"), None);
    assert_eq!(header(&headers, "x-codex-turn-state"), None);

    let artifact: Value = serde_json::from_slice(&fs::read(&result_path).expect("result artifact"))
        .expect("result JSON");
    assert_eq!(artifact["advertised"], true);
    assert_eq!(artifact["inner_id"], "gate-1");
    assert_eq!(artifact["result"]["id"], "r1/capability-outer");
    assert_eq!(artifact["result"]["name"], "web_screenshot");
    assert_eq!(
        artifact["result"]["error"],
        "web screenshot failed: provider could not resolve the screenshot call"
    );
    assert!(artifact["result"]["output"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("turn11view0")));
    assert_eq!(
        artifact["result"]["output"]["results"][0]["ref_id"],
        "turn11view0"
    );
    assert_eq!(
        artifact["result"]["provider_state"]["encrypted_output"],
        "opaque-screenshot-state"
    );
    assert!(artifact["result"]["output"].get("type").is_none());
}
