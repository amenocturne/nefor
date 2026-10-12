#![cfg(unix)]
//! Real engine VM (manager/compositor/gate), production MAG process and opaque native mock.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn run_case(case: &str) -> (PathBuf, Value, Vec<Value>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let bin = if target.is_absolute() {
        target
    } else {
        root.join(target)
    }
    .join("debug");
    for name in ["nefor", "mag-plugin", "mock-plugin", "tool-gate"] {
        assert!(
            bin.join(name).is_file(),
            "run just build-headless first: {name}"
        );
    }
    let dir = tempfile::Builder::new()
        .prefix(&format!("compaction-{case}-"))
        .tempdir_in(root.join("tmp"))
        .unwrap()
        .keep();
    let preamble = format!(
        "ROOT={}\nBIN={}\nDIR={}\nCASE={}\n",
        serde_json::to_string(&root).unwrap(),
        serde_json::to_string(&bin).unwrap(),
        serde_json::to_string(&dir).unwrap(),
        serde_json::to_string(case).unwrap()
    );
    fs::write(
        dir.join("init.lua"),
        format!("{preamble}dofile(ROOT..'/engine/tests/compaction_process/init.lua')\n"),
    )
    .unwrap();
    fs::write(
        dir.join("provider.lua"),
        format!("{preamble}dofile(ROOT..'/engine/tests/compaction_process/provider.lua')\n"),
    )
    .unwrap();
    let mut source =
        fs::read_to_string(root.join("engine/tests/compaction_process/program.mag")).unwrap();
    if case == "structured" {
        source = format!(
            "type Reply {{answer:String}}\n{}",
            source.replace(
                "agent<Unit, String, TextAnswer>",
                "agent<Unit, String, Reply>"
            )
        );
    } else if case == "dynamic" {
        source = format!("import nefor.dynamic.{{DynamicList}}\nimport nefor.{{result_map, traverse}}\n{}", source.replace("agent<Unit, String, TextAnswer>", "agent<Unit, String, DynamicList<String>>").replace("compile_graph(source(\"task\", \"begin\") >>> worker)", "let child = agent<Unit, String, TextAnswer>(\"expanded-child\", resolve, AgentConfig<Unit>{model:nil,system:\"expanded worker\",tools:[],tool_approval_policy:ToolApprovalPolicy.Default})\ncompile_graph(result_map(source(\"task\", \"begin\") >>> worker, traverse(\"workers\", child)))"));
    }
    fs::write(dir.join("program.mag"), source).unwrap();
    let child = Command::new(bin.join("nefor"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &dir)
        .env("NEFOR_LOG_STDERR", "1")
        .args([
            "--config",
            dir.to_str().unwrap(),
            "--data-dir",
            dir.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(dir.join("stderr.log")).unwrap(),
        ))
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(out) => out.unwrap(),
        Err(error) => {
            Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status()
                .unwrap();
            panic!("{error}: artifacts {}", dir.display())
        }
    };
    assert!(out.status.success(), "artifacts {}", dir.display());
    let result: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("result.json")).expect("result artifact"),
    )
    .unwrap();
    let native = fs::read_to_string(dir.join("native.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    eprintln!("{case} process witness: {}", dir.display());
    (dir, result, native)
}

fn assert_barriers(result: &Value) {
    let mut pending = HashSet::new();
    let mut recorded = HashMap::new();
    let mut delivery = false;
    let mut notice_commands = HashSet::new();
    let mut compactions = HashSet::new();
    let mut committed_ids = HashSet::new();
    for e in result["events"].as_array().unwrap() {
        let body = &e["body"];
        match body["kind"].as_str().unwrap_or("") {
            "conversation.fact.append" => {
                pending.insert(body["fact"]["event_id"].as_str().unwrap().to_owned());
            }
            "conversation.fact.recorded" => {
                pending.remove(body["event"]["event_id"].as_str().unwrap());
                if body["event"]["kind"] == "context_compaction_completed"
                    || body["event"]["kind"] == "context_compaction_failed"
                {
                    compactions.remove(body["event"]["request_id"].as_str().unwrap());
                    if body["event"]["kind"] == "context_compaction_completed" {
                        committed_ids.insert(body["event"]["request_id"].as_str().unwrap());
                    }
                }
                if body["event"]["kind"] == "tool_result_recorded" {
                    if let Ok(value) = serde_json::from_str::<Value>(
                        body["event"]["result"].as_str().unwrap_or(""),
                    ) {
                        if value["status"] == "compacted" {
                            assert!(
                                committed_ids.contains(value["request_id"].as_str().unwrap()),
                                "self result before canonical commit"
                            );
                            assert_eq!(value["epoch_id"], value["request_id"]);
                        }
                    }
                }
                recorded.insert(
                    body["event"]["conversation_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    body["event"]["sequence"].as_u64().unwrap(),
                );
                if body["event"]["kind"] == "context_notice_delivered" {
                    delivery = true;
                }
            }
            "conversation.provider.invoke.request" => {
                assert!(
                    notice_commands.is_empty(),
                    "model invoke before notice command ACK"
                );
                assert!(
                    compactions.is_empty(),
                    "model invoke at compaction admission rather than COMMIT"
                );
                assert!(
                    pending.is_empty(),
                    "invoke before preceding write ACK: {pending:?}"
                );
                let cid = body["conversation_id"].as_str().unwrap();
                assert!(body["context_watermark"].as_u64().unwrap() <= recorded[cid]);
            }
            "conversation.context.compact.request" => {
                if body["trigger"] == "self" {
                    assert!(delivery, "self before canonical delivery");
                }
                compactions.insert(body["request_id"].as_str().unwrap());
            }
            "conversation.context.notice.append"
            | "conversation.context.notice.bind"
            | "conversation.context.notice.acknowledge" => {
                notice_commands.insert(body["command_id"].as_str().unwrap());
            }
            "conversation.context.notice.accepted" | "conversation.context.notice.rejected" => {
                notice_commands.remove(body["command_id"].as_str().unwrap());
            }
            _ => {}
        }
    }
}

fn assert_pairing(messages: &[Value]) {
    let mut calls = HashSet::new();
    let mut paired = HashSet::new();
    for m in messages {
        if let Some(items) = m["tool_calls"].as_array() {
            for call in items {
                calls.insert(call["id"].as_str().unwrap());
            }
        }
        if m["role"] == "tool" {
            let id = m["tool_call_id"].as_str().unwrap();
            assert!(calls.contains(id), "orphan tool result {id}");
            assert!(paired.insert(id), "duplicate tool result {id}");
        }
    }
    assert_eq!(calls, paired, "incomplete tool pairing");
}

#[test]
fn production_compaction_process() {
    for case in [
        "soft",
        "hard",
        "disabled",
        "unsupported",
        "missing",
        "partial",
        "structured",
        "dynamic",
        "cancel",
    ] {
        let (dir, result, native) = run_case(case);
        let terminal = &result["result"];
        let expected = if case == "cancel" {
            "killed"
        } else if case == "missing" {
            "failed"
        } else {
            "completed"
        };
        if case != "missing" && case != "cancel" && case != "dynamic" {
            assert_eq!(
                terminal["result"]["value"]["constructor"],
                if case == "partial" { "Error" } else { "Ok" },
                "typed outcome"
            );
        }
        assert_eq!(
            terminal["status"],
            expected,
            "{}: {terminal}",
            dir.display()
        );
        if case == "dynamic" {
            assert_eq!(terminal["result"]["dynamic"]["count"], 1);
            assert!(
                native.last().unwrap()["body"]["request"]
                    .to_string()
                    .contains("expanded worker"),
                "dynamic child uses production lazy construction"
            );
        }
        assert_barriers(&result);
        let events = result["events"].as_array().unwrap();
        if !["hard", "missing"].contains(&case) {
            assert!(
                events.iter().any(|event| {
                    event["body"]["kind"] == "mag.arrival"
                        && event["body"]["wire"] == "generic-tool.ToolCalls"
                        && event["body"]["semantic_type"]["name"] == "nefor.wire.ToolCalls"
                        && event["body"]["value"]["value"]["calls"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                }),
                "self-only output must cross the real semantic boundary as [], not {{}}"
            );
        }
        for event in events {
            let body = &event["body"];
            if body["kind"] == "conversation.compaction.prepared"
                && body["outcome"]["regular_window"].is_number()
            {
                assert_eq!(
                    body["outcome"]["regular_window"], 100,
                    "policy uses regular, not advertised maximum 1000"
                );
            }
        }
        let mut seen = HashSet::new();
        let facts: Vec<_> = events
            .iter()
            .filter(|e| e["body"]["kind"] == "conversation.fact.recorded")
            .map(|e| &e["body"]["event"])
            .filter(|f| seen.insert(f["event_id"].as_str().unwrap()))
            .collect();
        let commits: Vec<_> = facts
            .iter()
            .filter(|f| f["kind"] == "context_compaction_completed")
            .collect();
        let compact_count = native.iter().filter(|e| e["kind"] == "compact").count();
        let requests: Vec<_> = native.iter().filter(|e| e["kind"] == "request").collect();
        if ["soft", "hard", "structured", "dynamic"].contains(&case) {
            assert_eq!(commits.len(), 1, "{}: canonical COMMIT", dir.display());
            assert_eq!(compact_count, 1, "coalesced native operation");
            let last = &requests[if case == "dynamic" {
                2
            } else {
                requests.len() - 1
            }]["body"]["request"];
            let context = &last["conversation_context"];
            assert!(context["compaction"]["checkpoint"]["artifact"]["items"].is_array());
            if let Some(tail) = context["tail_messages"].as_array() {
                assert_pairing(tail);
            } else {
                assert_eq!(context["tail_messages"], serde_json::json!({}));
            }
            if case != "hard" {
                assert!(
                    facts
                        .iter()
                        .any(|f| f["kind"] == "tool_result_recorded"
                            && f["result"] == "sibling done"),
                    "real sibling result must be accepted"
                );
                assert!(requests[1]["body"]["request"]
                    .to_string()
                    .contains("Context usage is 65%"));
                let frozen: Vec<_> = native
                    .iter()
                    .filter(|e| e["kind"] == "frozen_message")
                    .collect();
                assert!(
                    !serde_json::to_string(&frozen).unwrap().contains("self1"),
                    "batch excluded from frozen prefix"
                );
                assert_eq!(
                    requests.len(),
                    if case == "soft" {
                        3
                    } else if case == "dynamic" {
                        5
                    } else {
                        4
                    }
                );
            } else {
                assert_eq!(requests.len(), 2);
                assert!(
                    facts
                        .iter()
                        .any(|f| f["kind"] == "context_compaction_requested"
                            && f["trigger"] == "hard")
                );
            }
        } else {
            assert!(commits.is_empty(), "unexpected commit: {}", dir.display());
            if case != "cancel" {
                assert_eq!(compact_count, 0);
            }
            let text = result.to_string();
            match case {
                "disabled" => assert!(text.contains("compaction_notification_required")),
                "unsupported" => assert!(text.contains("compaction_unsupported")),
                "missing" => {
                    assert!(requests.is_empty());
                    assert!(text.contains("compaction_capacity_missing"));
                }
                "partial" => {
                    assert_eq!(requests.len(), 2);
                    assert!(facts
                        .iter()
                        .any(|f| f["kind"] == "context_notice_delivered"));
                }
                "cancel" => {
                    assert!(text.contains("compaction_interrupted"));
                    assert_eq!(native.iter().filter(|e| e["kind"] == "cancel").count(), 1);
                    assert_eq!(requests.len(), 2, "no resume after cancel");
                    let final_trace = fs::read_to_string(dir.join("trace.jsonl")).unwrap();
                    assert!(
                        !final_trace.contains("context_compaction_completed"),
                        "late native COMMIT must not become canonical"
                    );
                }
                _ => unreachable!(),
            }
        }
    }
}
