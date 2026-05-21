//! Stage 1 end-to-end integration test — chat.input.submit → chat.message.append.
//!
//! Mirrors `combinators_slice1.rs`'s harness shape (in-process Lua + real
//! broker + spawned plugin subprocesses + duplex driver) but exercises the
//! full Stage 1 wire (per parent spec §6.1):
//!
//! ```text
//!   driver(chat.input.submit) → agentic-loop → tool.invoke{name=spawn_graph}
//!     → combinators.query → nefor-combinators → combinators.query.result
//!     → reasoner-graph dispatches tool.invoke { name=provider-wrapper }
//!       → reasoners actor → ollama.chat.{create,append,complete}
//!     → mock-plugin (impersonating openai-provider as "ollama") returns
//!       chat.complete.result with tool_calls=[spawn_graph(...)]
//!     → openai-provider wrapper → tool.result { id=firing_id, result }
//!     → reasoner-graph invokes tool_split → routes to tool-executor
//!       → tool-gate.tool.invoke → tool-gate forwards as
//!         spawn-graph-tool.tool.invoke (the virtual source name
//!         spawn_graph.lua registers under)
//!       → tool-gate wrapper (on tool-gate's egress chain) →
//!         tool.invoke{name=spawn_graph} (sub-graph)
//!     → sub-graph runs (also against mock-plugin)
//!     → tool.result { id=sub_run_id, result } → spawn_graph relay
//!     → wrap re-fires → mock-plugin returns final text → tool_split routes
//!       to terminal escape → tool.result { id=outer_run_id, result }
//!     → agentic-loop notices our run is complete → chat.message.append → driver
//! ```
//!
//! mock-plugin stands in for openai-provider — its Lua script is
//! scripted to return tool_calls on the first chat.complete and a final
//! text on the second. Sub-graph dummy-node completes always return
//! plain text.
//!
//! Assertions are deliberately bookend-shaped:
//!  * driver's `chat.input.submit` round-trips into a `chat.message.append`
//!    with the expected final text;
//!  * a `combinators.query` was emitted at submit time;
//!  * a `tool-gate.tool.invoke` for `spawn_graph` was issued during the run;
//!  * tool-gate forwarded it as `spawn-graph-tool.tool.invoke`;
//!  * spawn_graph fired a sub-graph (>=2 tool.invoke{name=spawn_graph} entries).
//!
//! Gated `#[ignore]`; run with
//! `cargo test -p nefor --test stage1_e2e -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nefor::events::EventBus;
use nefor::lua::bindings::EngineOps;
use nefor::lua::LuaHost;
use nefor::ncp::runner::PluginRoot;
use nefor::ncp::spawn::{PluginKind, PluginSpec};
use nefor::ncp::transport::Transport;
use nefor::ncp::{spawn_plugin, Broker, BrokerOps, BrokerShared, PluginRegistry};
use nefor_protocol::PluginName;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// Hard cap on the full submit → assistant-message round trip. Generous —
/// we spawn 7 subprocesses plus a Lua VM, walk three full graph turns,
/// and route a sub-graph in the middle.
const RESULT_TIMEOUT: Duration = Duration::from_secs(25);

/// Quiet window after `ready_ok` before driving traffic — lets every
/// plugin's startup chatter (`combinators.register`, `*.hello`,
/// `tool-gate.hello`, `tools.advertise`, …) settle so the orchestrator's
/// submit-time combinator query hits a populated registry.
const QUIET_WINDOW: Duration = Duration::from_secs(2);

/// Per-line read timeout while scanning incoming traffic.
const LINE_READ_TIMEOUT: Duration = Duration::from_millis(500);

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(PathBuf::from)
        .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
}

fn debug_dir() -> PathBuf {
    workspace_root().join("target").join("debug")
}

fn starter_dir() -> PathBuf {
    workspace_root().join("starter")
}

fn lua_dir() -> PathBuf {
    workspace_root().join("lua")
}

/// Build all plugin binaries we depend on. No-op on a warm cache.
fn ensure_binaries_built() {
    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg("nefor-combinators-plugin")
        .arg("-p")
        .arg("generic-provider")
        .arg("-p")
        .arg("generic-tool")
        .arg("-p")
        .arg("reasoner-graph")
        .arg("-p")
        .arg("tool-gate-plugin")
        .arg("-p")
        .arg("basic-tools-plugin")
        .arg("-p")
        .arg("mock-plugin")
        .current_dir(workspace_root())
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "failed to build plugin binaries");
}

/// Driver side of the in-memory duplex. Used twice — once impersonating
/// `nefor-tui`, once observing `mock-plugin` traffic in the assertions.
struct Driver {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
}

impl Driver {
    async fn send_line(&mut self, line: &str) {
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("driver write");
        if !line.ends_with('\n') {
            self.writer.write_all(b"\n").await.expect("driver newline");
        }
        self.writer.flush().await.expect("driver flush");
    }

    async fn recv_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match timeout(LINE_READ_TIMEOUT, self.reader.read_line(&mut line)).await {
            Ok(Ok(0)) => None,
            Ok(Ok(_)) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
            Ok(Err(e)) => panic!("driver read error: {e}"),
            Err(_) => None,
        }
    }
}

fn make_driver_transport() -> (Driver, Transport) {
    let (driver_side, broker_side) = duplex(128 * 1024);
    let (broker_read, broker_write) = tokio::io::split(broker_side);
    let (driver_read, driver_write) = tokio::io::split(driver_side);
    let transport = Transport {
        reader: Box::pin(broker_read),
        writer: Box::pin(broker_write),
        stderr: None,
        exit: None,
    };
    let driver = Driver {
        writer: driver_write,
        reader: BufReader::new(driver_read),
    };
    (driver, transport)
}

/// Write a test init.lua under `dir`. Wires:
///   * `package.path` so `require("core.ncp")` etc. resolve to `starter/`.
///   * `dispatch` hook → `ncp.dispatch`.
///   * Transforms registered for "nefor-tui", "ollama", "reasoner-graph",
///     "tool-gate" via `ncp._test_set_transforms` — the test attaches the
///     plugins itself, so we skip `ncp.spawn` (it would try to fork a
///     binary that's already been spawned).
fn write_test_init_lua(dir: &Path) {
    let starter = starter_dir();
    let starter_str = format!("{:?}", starter.display().to_string());
    let lua_root = lua_dir();
    let lua_root_str = format!("{:?}", lua_root.display().to_string());
    let rg_plugin_lua = workspace_root()
        .join("plugins")
        .join("reasoner-graph")
        .join("lua");
    let rg_plugin_lua_str = format!("{:?}", rg_plugin_lua.display().to_string());
    // Use a templated string with a placeholder that's unlikely to collide
    // with Lua syntax — we substitute @@STARTER@@ / @@LUA_ROOT@@ / @@RG@@
    // rather than wrestle with format!'s `{` escaping rules across a long
    // Lua program.
    let template = r#"-- Stage 1 e2e test init.lua.
package.path = table.concat({
  @@STARTER@@ .. "/?.lua",
  @@STARTER@@ .. "/?/init.lua",
  @@RG@@ .. "/?.lua",
  @@RG@@ .. "/?/init.lua",
  @@LUA_ROOT@@ .. "/?.lua",
  @@LUA_ROOT@@ .. "/?/init.lua",
  package.path,
}, ";")

local ncp              = require("core.ncp")
local agentic_workflow = require("agentic_workflow")

function dispatch(current_log)
  ncp.dispatch(current_log)
end

local PROVIDER = "ollama"
local MODEL    = "mock-model"

agentic_workflow.setup { provider = PROVIDER, model = MODEL }

-- Provider chain — agentic_workflow.for_provider already composes the
-- rg-style + chat-contract transforms internally.
local provider_chain = agentic_workflow.for_provider(PROVIDER, { static_token = "ollama-local" })
ncp._test_set_transforms(PROVIDER, {
  from_plugin = provider_chain.from_plugin,
  to_plugin   = provider_chain.to_plugin,
})

local rg_chain = agentic_workflow.for_reasoner_graph()
ncp._test_set_transforms("reasoner-graph", {
  from_plugin = rg_chain.from_plugin,
})

local gate_chain = agentic_workflow.for_tool_gate("tool-gate")
ncp._test_set_transforms("tool-gate", {
  from_plugin = gate_chain.from_plugin,
})

local chat_chain = agentic_workflow.for_chat()
ncp._test_set_transforms("nefor-tui", {
  from_plugin = chat_chain.from_plugin,
})
"#;
    let src = template
        .replace("@@STARTER@@", &starter_str)
        .replace("@@LUA_ROOT@@", &lua_root_str)
        .replace("@@RG@@", &rg_plugin_lua_str);
    std::fs::write(dir.join("init.lua"), src).expect("write test init.lua");
}

/// Mock-plugin script. Stands in for openai-provider under wire name
/// `ollama`. Tracks per-chat turn counts. The first `ollama.chat.complete`
/// for any given chat returns tool_calls=[spawn_graph(...)] when that
/// chat is the FIRST chat created (the orchestrator wrap chat); every
/// other chat (sub-graph dummy nodes) returns plain text. The wrap chat's
/// SECOND complete (after the spawn_graph tool result loops back) returns
/// the final text, which the orchestrator then renders to the driver.
const MOCK_SCRIPT: &str = r#"
local SUB_GRAPH = {
  nodes = {
    { id = "octo",  reasoner = "dummy", args = { provider = "ollama", model = "mock-model", prompt = "summarise octopuses" } },
    { id = "light", reasoner = "dummy", args = { provider = "ollama", model = "mock-model", prompt = "summarise lighthouses" } },
  },
  edges = {},
}

-- chat_id → completion count.
local completes = {}
-- The first chat we see is the orchestrator's wrap chat. Subsequent
-- chats are sub-graph dummy nodes.
local wrap_chat_id = nil

local function emit_complete_result(chat_id, text, tool_calls)
  local output = { text = text }
  if tool_calls then
    output.tool_calls = tool_calls
  end
  output.finish_reason = "stop"
  output.usage = { prompt_tokens = 0, completion_tokens = 0, model = "mock-model" }
  nefor.emit_raw("ollama.chat.complete.result", {
    chat_id = chat_id,
    output  = output,
  })
end

-- chat.create / chat.append are no-ops in the mock — we only care
-- about chat.complete (which is what triggers a model turn).
nefor.on("ollama.chat.create", function(body)
  if wrap_chat_id == nil then
    wrap_chat_id = body.chat_id
  end
end)

nefor.on("ollama.chat.append", function(_body)
  -- no-op
end)

nefor.on("ollama.chat.complete", function(body)
  local chat_id = body.chat_id
  completes[chat_id] = (completes[chat_id] or 0) + 1
  local n = completes[chat_id]

  if chat_id == wrap_chat_id and n == 1 then
    -- First wrap turn: emit a spawn_graph tool call.
    emit_complete_result(chat_id, "", {
      {
        id        = "call-1",
        name      = "spawn_graph",
        arguments = {
          graph           = SUB_GRAPH,
          on_node_failure = "abort",
        },
      },
    })
  elseif chat_id == wrap_chat_id then
    -- Subsequent wrap turn (after tool result loops back): final text.
    emit_complete_result(chat_id, "FINAL: octopuses + lighthouses summarised", nil)
  else
    -- Any sub-graph dummy-node chat: just return plain text.
    emit_complete_result(chat_id, "(mock dummy answer for " .. tostring(chat_id) .. ")", nil)
  end
end)

-- The provider adapter pushes a synthetic `ollama.auth.set` into the
-- bus once it sees `ollama.ready`. We don't need it; absorb silently.
nefor.on("ollama.auth.set", function(_body) end)

nefor.on_ready_ok(function()
  -- Announce a synthetic <prefix>.hello / <prefix>.ready so the
  -- openai_provider_adapter's lifecycle hooks fire (they would
  -- otherwise be inert — they trigger on the real provider's
  -- startup chatter). Without these, the static-token unlock path
  -- doesn't run, but mock-plugin doesn't enforce auth so that's fine —
  -- they're emitted for completeness so the wire shape matches what
  -- the real openai-provider would send.
  nefor.emit_raw("ollama.hello", { name = "ollama", model = "mock-model" })
  nefor.emit_raw("ollama.ready", { name = "ollama" })
end)
"#;

fn write_mock_script(dir: &Path) -> PathBuf {
    let path = dir.join("mock_provider.lua");
    std::fs::write(&path, MOCK_SCRIPT).expect("write mock script");
    path
}

async fn build_host(shared: Arc<Mutex<BrokerShared>>, config_dir: &Path) -> LuaHost {
    let bus = Arc::new(EventBus::new());
    let plugins = Arc::new(Mutex::new(PluginRegistry::new()));
    let ops: Arc<dyn EngineOps> = Arc::new(BrokerOps::new(Arc::clone(&shared)));
    let data_dir = nefor::paths::DataDir::new(PathBuf::from("/var/empty/stage1-e2e-data"));
    let mut host = LuaHost::new(bus, plugins, ops, data_dir).expect("lua host");
    host.load_init(&config_dir.join("init.lua"))
        .await
        .expect("load init.lua");
    host.cache_dispatch().expect("cache dispatch");
    host
}

fn parse_json_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

/// Drive the standard NCP handshake from `driver`.
async fn handshake(driver: &mut Driver) {
    driver
        .send_line(r#"{"type":"system","body":{"kind":"ready","protocol_version":"0.1"}}"#)
        .await;

    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = ready_deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for ready_ok");
        let line = match timeout(remaining, driver.recv_line()).await {
            Ok(Some(line)) => line,
            Ok(None) => continue,
            Err(_) => panic!("timed out waiting for ready_ok"),
        };
        if let Some(v) = parse_json_line(&line) {
            if v.get("type").and_then(Value::as_str) == Some("system") {
                if let Some(body) = v.get("body") {
                    if body.get("kind").and_then(Value::as_str) == Some("ready_ok") {
                        break;
                    }
                }
            }
        }
    }
}

/// Drain everything currently flowing into `driver` for `window`. Pops the
/// stream so subsequent reads start fresh.
async fn drain(driver: &mut Driver, window: Duration) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + window;
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, driver.recv_line()).await {
            Ok(Some(line)) => {
                if let Some(v) = parse_json_line(&line) {
                    out.push(v);
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
// shared_guard holds the std::sync::Mutex briefly while we read the event
// log; the awaits at the end of the test are after we drop it. Clippy
// can't see through the explicit drop, so suppress at the function level.
#[allow(clippy::await_holding_lock)]
async fn stage1_chat_input_submit_round_trips_to_assistant_message() {
    ensure_binaries_built();

    let tmp = TempDir::new().expect("tempdir");
    write_test_init_lua(tmp.path());
    let mock_script = write_mock_script(tmp.path());

    let shared = Arc::new(Mutex::new(BrokerShared::new()));
    let host = build_host(Arc::clone(&shared), tmp.path()).await;

    // Plugin root must be a directory containing one subdir per plugin
    // name (the runner uses `<root>/<name>/` as cwd). The repo's
    // `plugins/` has dirs for the real plugins; create empty dirs in a
    // tempdir-based root for the `ollama` mock and the driver-side
    // `nefor-tui`.
    //
    // No phantom plugins: Lua-resident reasoner types (provider-wrapper,
    // tool-executor, adapter, terminal) seed the reasoner-graph binary's
    // peer-set by emitting one `<name>.ready` envelope per type with
    // `from = <name>` from the `reasoners` actor on first
    // `reasoner-graph.ready`. See plugins/reasoner-graph/src/main.rs
    // (`track_peer`) and starter/reasoners/init.lua (`seed_peer_set`).
    let plugin_root_dir = tmp.path().join("plugins");
    std::fs::create_dir_all(&plugin_root_dir).expect("plugin root dir");
    for name in [
        "nefor-combinators",
        "generic-provider",
        "generic-tool",
        "reasoner-graph",
        "tool-gate",
        "basic-tools",
        "ollama",
        "nefor-tui",
    ] {
        std::fs::create_dir_all(plugin_root_dir.join(name)).expect("plugin cwd");
    }
    let root = PluginRoot::new(plugin_root_dir);
    let debug = debug_dir();

    // Plugin specs. The mock-plugin is spawned under wire name "ollama"
    // (the broker stamps `from = ollama` on every line it emits) so
    // events emitted via `nefor.emit_raw("ollama.<sub>", ...)` look
    // identical to the real openai-provider's `ollama.*` namespace.
    let combinators_spec = PluginSpec {
        name: PluginName::new("nefor-combinators").expect("valid name"),
        kind: PluginKind::Command(vec![debug.join("nefor-combinators").display().to_string()]),
    };
    let generic_provider_spec = PluginSpec {
        name: PluginName::new("generic-provider").expect("valid name"),
        kind: PluginKind::Command(vec![debug.join("generic-provider").display().to_string()]),
    };
    let generic_tool_spec = PluginSpec {
        name: PluginName::new("generic-tool").expect("valid name"),
        kind: PluginKind::Command(vec![debug.join("generic-tool").display().to_string()]),
    };
    let rg_spec = PluginSpec {
        name: PluginName::new("reasoner-graph").expect("valid name"),
        kind: PluginKind::Command(vec![debug.join("reasoner-graph").display().to_string()]),
    };
    let gate_spec = PluginSpec {
        name: PluginName::new("tool-gate").expect("valid name"),
        kind: PluginKind::Command(vec![
            debug.join("tool-gate").display().to_string(),
            // `auto` = "auto-allow unlisted tools" (the gate's no-prompt
            // mode). The starter's prod init.lua passes `prompt` instead
            // and routes prompts to nefor-tui — but in the test we have
            // no real chat to prompt, so auto-approve everything.
            "--default".into(),
            "auto".into(),
        ]),
    };
    let basic_tools_spec = PluginSpec {
        name: PluginName::new("basic-tools").expect("valid name"),
        kind: PluginKind::Command(vec![
            debug.join("basic-tools").display().to_string(),
            "--gate".into(),
            "tool-gate".into(),
        ]),
    };
    let ollama_spec = PluginSpec {
        name: PluginName::new("ollama").expect("valid name"),
        kind: PluginKind::Command(vec![
            debug.join("mock-plugin").display().to_string(),
            "--script".into(),
            mock_script.display().to_string(),
        ]),
    };

    // Spawn order matches §6.1's policy: registry → canonical → reasoner →
    // gate → tools → provider. Order isn't load-bearing here (replay-on-
    // attach covers late attachers) but matching the reference helps if
    // a real timing dependency surfaces later.
    let combinators_t = spawn_plugin(&combinators_spec, &root).expect("spawn combinators");
    let generic_provider_t =
        spawn_plugin(&generic_provider_spec, &root).expect("spawn generic-provider");
    let generic_tool_t = spawn_plugin(&generic_tool_spec, &root).expect("spawn generic-tool");
    let rg_t = spawn_plugin(&rg_spec, &root).expect("spawn reasoner-graph");
    let gate_t = spawn_plugin(&gate_spec, &root).expect("spawn tool-gate");
    let basic_tools_t = spawn_plugin(&basic_tools_spec, &root).expect("spawn basic-tools");
    let ollama_t = spawn_plugin(&ollama_spec, &root).expect("spawn mock-plugin (ollama)");

    let (mut driver, driver_transport) = make_driver_transport();

    let mut broker = Broker::new(Arc::clone(&shared), host);
    broker.attach_transport(combinators_t, combinators_spec.name.clone());
    broker.attach_transport(generic_provider_t, generic_provider_spec.name.clone());
    broker.attach_transport(generic_tool_t, generic_tool_spec.name.clone());
    broker.attach_transport(rg_t, rg_spec.name.clone());
    broker.attach_transport(gate_t, gate_spec.name.clone());
    broker.attach_transport(basic_tools_t, basic_tools_spec.name.clone());
    broker.attach_transport(ollama_t, ollama_spec.name.clone());
    broker.attach_transport(
        driver_transport,
        PluginName::new("nefor-tui").expect("valid name"),
    );

    let shutdown = broker.shutdown_handle();
    let broker_task = tokio::spawn(broker.run());

    handshake(&mut driver).await;

    // Wait until every required peer is observable in the event log.
    // Both real plugins and Lua-resident reasoner types are observed via
    // the `from` field on emitted envelopes. The reasoners actor seeds
    // the peer-set with `<name>.ready { from=<name> }` envelopes per
    // resident reasoner type on first `reasoner-graph.ready`, so the
    // same scan covers both classes.
    let required_real_peers = [
        "ollama",
        "tool-gate",
        "basic-tools",
        "reasoner-graph",
        "nefor-combinators",
        "generic-provider",
        "generic-tool",
        // Lua-resident reasoner types — emitted as `<name>.ready` with
        // `from = <name>`; checking the same `from` set as real peers
        // is sufficient.
        "provider-wrapper",
        "tool-executor",
        "adapter",
        "terminal",
    ];
    {
        let peers_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let seen_from: std::collections::HashSet<String> = {
                let guard = shared.lock().expect("shared lock");
                let mut from = std::collections::HashSet::new();
                for entry in guard.event_log.iter() {
                    if let Ok(v) = serde_json::from_str::<Value>(&entry.payload) {
                        if let Some(f) = v.get("from").and_then(Value::as_str) {
                            from.insert(f.to_owned());
                        }
                    }
                }
                from
            };
            let missing_real: Vec<&str> = required_real_peers
                .iter()
                .copied()
                .filter(|p| !seen_from.contains(*p))
                .collect();
            if missing_real.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < peers_deadline,
                "timed out waiting for peers to ready: missing={missing_real:?}; \
                 saw from={seen_from:?}"
            );
            // Keep draining the driver so the read buffer doesn't stall
            // the broker's writer queue.
            let _ = drain(&mut driver, Duration::from_millis(100)).await;
        }
    }

    // Final quiet-window drain to absorb late chatter.
    let _startup = drain(&mut driver, QUIET_WINDOW).await;

    // Submit the chat-style request that nefor-tui would have emitted.
    let submit = json!({
        "type": "event",
        "body": {
            "kind": "chat.input.submit",
            "text": "summarise octopuses and lighthouses",
        },
    });
    driver.send_line(&submit.to_string()).await;

    // Collect every envelope until either the assistant message arrives
    // or we time out.
    let deadline = tokio::time::Instant::now() + RESULT_TIMEOUT;
    let mut all_traffic: Vec<Value> = Vec::new();
    let mut assistant_text: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = match timeout(remaining, driver.recv_line()).await {
            Ok(Some(line)) => line,
            Ok(None) => continue,
            Err(_) => break,
        };
        let Some(v) = parse_json_line(&line) else {
            continue;
        };
        let kind = v
            .get("body")
            .and_then(|b| b.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let role = v
            .get("body")
            .and_then(|b| b.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        all_traffic.push(v.clone());
        if kind == "chat.message.append" && role == "assistant" {
            assistant_text = v
                .get("body")
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            break;
        }
    }

    // ---- Read the in-memory event log for structural assertions -----
    //
    // The driver-side cuts only see traffic targeted at "nefor-tui" or
    // broadcast — that excludes targeted-only envelopes like
    // `tool-gate.tool.invoke` (delivered to tool-gate via the
    // prefix-targeting routing in ncp.lua). For those we read the
    // broker's in-memory `event_log` via shared state.
    let log_kinds: Vec<String> = {
        let guard = shared.lock().expect("shared lock");
        guard
            .event_log
            .iter()
            .filter_map(|entry| {
                serde_json::from_str::<Value>(&entry.payload)
                    .ok()
                    .and_then(|v| {
                        v.get("body")
                            .and_then(|b| b.get("kind"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
            })
            .collect()
    };

    // ---- Structural intermediate signals (always asserted) -----------
    assert!(
        log_kinds.iter().any(|k| k == "combinators.query"),
        "expected at least one combinators.query at submit time; \
         saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds.iter().any(|k| k == "ollama.chat.create"),
        "expected reasoner_graph_adapter to drive ollama.chat.create on \
         the wrap node's first firing; saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds.iter().any(|k| k == "ollama.chat.complete.result"),
        "expected the mock provider to reply with chat.complete.result; \
         saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds.iter().any(|k| k == "combinators.invoke"),
        "expected reasoner-graph to invoke tool_split after the wrap \
         node returned tool_calls; saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds.iter().any(|k| k == "tool.invoke"),
        "expected at least one tool.invoke (canonical dispatch surface) \
         to fire — including the tool-executor's invoke for the \
         spawn_graph call; saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds.iter().any(|k| k == "tool-gate.tool.invoke"),
        "expected the tool-executor to dispatch via tool-gate.tool.invoke \
         for spawn_graph; saw kinds: {log_kinds:?}"
    );
    assert!(
        log_kinds
            .iter()
            .any(|k| k == "spawn-graph-tool.tool.invoke"),
        "expected tool-gate to forward the spawn_graph invoke as \
         spawn-graph-tool.tool.invoke (source: spawn-graph-tool, the \
         virtual source name spawn_graph.lua advertises under); saw \
         kinds: {log_kinds:?}"
    );
    // spawn_graph.lua intercepts spawn-graph-tool.tool.invoke on
    // tool-gate's egress and fires a nested tool.invoke{name=spawn_graph}. We
    // expect at least TWO `tool.invoke { name="spawn_graph" }` entries —
    // the outer run started by chat_orchestrator and the sub-graph
    // started by spawn_graph.
    let spawn_graph_invoke_count: usize = {
        let guard = shared.lock().expect("shared lock");
        guard
            .event_log
            .iter()
            .filter(|entry| {
                let Ok(v) = serde_json::from_str::<Value>(&entry.payload) else {
                    return false;
                };
                let body = v.get("body");
                body.and_then(|b| b.get("kind")).and_then(Value::as_str) == Some("tool.invoke")
                    && body.and_then(|b| b.get("name")).and_then(Value::as_str)
                        == Some("spawn_graph")
            })
            .count()
    };
    assert!(
        spawn_graph_invoke_count >= 2,
        "expected spawn_graph to fire a sub-graph tool.invoke{{name=spawn_graph}} \
         (so >=2 total such entries); saw {spawn_graph_invoke_count}; \
         log kinds: {log_kinds:?}"
    );

    // ---- Bookend assertion --------------------------------------------
    //
    // The full Stage 1 wire round-trips: chat.input.submit → outer
    // tool.invoke{name=spawn_graph} → wrap node → tool_split → tool-gate →
    // spawn_graph → sub-graph → tool.result (sub run-close) → loops back
    // → wrap re-fires → final text → terminal escape →
    // tool.result (outer run-close).
    //
    // chat_orchestrator does NOT emit chat.message.append on success —
    // streaming via openai-provider's chat.stream.delta + stream.end
    // already shows the assistant message in nefor-tui. Re-emitting
    // would duplicate the bubble. So we assert on the run's terminal
    // wire shape: an outer `tool.result { id=run_id, result: { status:
    // "success", results } }` whose `results.terminal.output.text`
    // contains "FINAL". Per Phase 3b's `synthesize_node_result`, the
    // run-close packs the prior `graph.run_complete` shape verbatim
    // into `result`.
    let _ = assistant_text; // Suppress unused warning if we keep the channel observation.
    let shared_guard = shared.lock().expect("shared lock");
    let outer_complete = shared_guard
        .event_log
        .iter()
        .filter_map(|entry| serde_json::from_str::<Value>(&entry.payload).ok())
        .filter(|v| {
            v.get("body")
                .and_then(|b| b.get("kind"))
                .and_then(Value::as_str)
                == Some("tool.result")
        })
        .find(|v| {
            // Outer run is the FIRST tool.result whose result.results
            // carries a `terminal` entry (the orchestrator template);
            // sub-graphs from spawn_graph don't have a "terminal" node id.
            v.get("body")
                .and_then(|b| b.get("result"))
                .and_then(|r| r.get("results"))
                .and_then(|r| r.get("terminal"))
                .is_some()
        })
        .unwrap_or_else(|| {
            panic!(
                "no outer tool.result envelope with a terminal result; \
                 log kinds: {log_kinds:?}"
            )
        });
    let status = outer_complete
        .get("body")
        .and_then(|b| b.get("result"))
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        status, "success",
        "outer run did not succeed: {outer_complete:?}"
    );
    let text = outer_complete
        .get("body")
        .and_then(|b| b.get("result"))
        .and_then(|r| r.get("results"))
        .and_then(|r| r.get("terminal"))
        .and_then(|t| t.get("output"))
        .and_then(|o| o.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("terminal output missing text: {outer_complete:?}"));
    assert!(
        text.contains("FINAL"),
        "terminal text mismatch: {text:?}; full log kinds: {log_kinds:?}"
    );
    drop(shared_guard);

    shutdown.shutdown(2000).await;
    let _ = timeout(Duration::from_secs(15), broker_task)
        .await
        .expect("broker task exits within grace");
}
