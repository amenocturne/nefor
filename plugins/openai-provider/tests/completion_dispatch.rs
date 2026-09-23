use std::process::Stdio;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, SystemBody, Timestamp};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

async fn write_envelope(stdin: &mut ChildStdin, envelope: Envelope) {
    stdin
        .write_all(envelope.to_line().as_bytes())
        .await
        .expect("write envelope");
    stdin.write_all(b"\n").await.expect("write newline");
    stdin.flush().await.expect("flush envelope");
}

async fn read_outgoing(reader: &mut BufReader<ChildStdout>) -> PluginOutgoing {
    let mut line = String::new();
    timeout(WAIT, reader.read_line(&mut line))
        .await
        .expect("provider output timeout")
        .expect("read provider output");
    PluginOutgoing::parse_line(line.trim_end()).expect("valid provider output")
}

fn event_body(outgoing: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &outgoing.body {
        Body::Event(body) => Some(body),
        Body::System(_) => None,
    }
}

async fn spawn_provider(base_url: &str) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_openai-provider"))
        .args([
            "--name",
            "fixture",
            "--base-url",
            base_url,
            "--model",
            "fixture-model",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn provider");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    assert!(matches!(
        read_outgoing(&mut stdout).await.body,
        Body::System(SystemBody::Ready { .. })
    ));
    write_envelope(
        &mut stdin,
        Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "test".into(),
            },
        ),
    )
    .await;
    (child, stdin, stdout)
}

async fn send_completion(stdin: &mut ChildStdin, request_id: &str, additions: Value) {
    let body = json!({
        "kind": "fixture.completion.request",
        "request_id": request_id,
        "messages": [{"role": "user", "content": "sanitized fixture"}],
        "request_additions": additions,
    })
    .as_object()
    .expect("object")
    .clone();
    write_envelope(
        stdin,
        Envelope::event(
            PluginName::new("test-caller").expect("plugin name"),
            Timestamp::now(),
            body,
        ),
    )
    .await;
}

async fn send_completion_messages(stdin: &mut ChildStdin, request_id: &str, messages: Value) {
    let body = json!({
        "kind": "fixture.completion.request",
        "request_id": request_id,
        "messages": messages,
    })
    .as_object()
    .expect("object")
    .clone();
    write_envelope(
        stdin,
        Envelope::event(
            PluginName::new("test-caller").expect("plugin name"),
            Timestamp::now(),
            body,
        ),
    )
    .await;
}

async fn next_completion_event(
    reader: &mut BufReader<ChildStdout>,
    request_id: &str,
) -> Map<String, Value> {
    loop {
        let outgoing = read_outgoing(reader).await;
        if let Some(body) = event_body(&outgoing) {
            if body.get("kind").and_then(Value::as_str) == Some("fixture.completion.event")
                && body.get("request_id").and_then(Value::as_str) == Some(request_id)
            {
                return body.clone();
            }
        }
    }
}

async fn read_request_json(stream: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")?
                    .parse::<usize>()
                    .ok()
            })
            .expect("content length");
        let start = header_end + 4;
        if bytes.len() >= start + length {
            return serde_json::from_slice(&bytes[start..start + length]).expect("request JSON");
        }
    }
}

#[tokio::test]
async fn direct_completion_dispatch_preserves_usage_id_and_request_additions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request_json(&mut stream).await;
        assert_eq!(request["metadata"], json!({"fixture": "sanitized"}));
        assert_eq!(request["provider_hint"], "fixture-route");
        let events = concat!(
            "data: {\"id\":\"completion-fixture\",\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: {\"id\":\"completion-fixture\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"completion-fixture\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"total_tokens\":14,\"vendor_detail\":{\"cached\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", events.len(), events);
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });
    let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
    send_completion(
        &mut stdin,
        "request-fixture",
        json!({
            "metadata": {"fixture": "sanitized"},
            "provider_hint": "fixture-route"
        }),
    )
    .await;

    let mut events = Vec::new();
    loop {
        let event = next_completion_event(&mut stdout, "request-fixture").await;
        let terminal = event.get("event").and_then(Value::as_str) == Some("completed");
        events.push(event);
        if terminal {
            break;
        }
    }
    server.await.expect("server");
    let usage_events: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "usage")
        .collect();
    assert_eq!(usage_events.len(), 1, "exactly one terminal usage event");
    assert_eq!(usage_events[0]["completion_id"], "completion-fixture");
    assert_eq!(
        usage_events[0]["usage"],
        json!({
            "prompt_tokens": 11,
            "completion_tokens": 3,
            "total_tokens": 14,
            "extensions": {"vendor_detail": {"cached": 4}}
        })
    );
    let completed = events
        .iter()
        .find(|event| event["event"] == "completed")
        .expect("completed");
    assert_eq!(completed["completion_id"], "completion-fixture");
    child.kill().await.expect("kill provider");
}

#[tokio::test]
async fn direct_completion_sends_empty_and_whitespace_tool_results_exactly() {
    for (request_id, content) in [
        ("empty-tool-result", ""),
        ("whitespace-tool-result", " \n\t"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let expected_content = content.to_owned();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_request_json(&mut stream).await;
            assert_eq!(
                request["messages"],
                json!([
                    {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{\"path\":\"empty.txt\"}"}
                        }]
                    },
                    {"role": "tool", "tool_call_id": "call-1", "content": expected_content}
                ])
            );
            let events = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });
        let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
        send_completion_messages(
            &mut stdin,
            request_id,
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"empty.txt\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call-1", "content": content}
            ]),
        )
        .await;

        loop {
            let event = next_completion_event(&mut stdout, request_id).await;
            if event["event"] == "completed" {
                assert_eq!(event["text"], "done");
                break;
            }
        }
        server.await.expect("server");
        child.kill().await.expect("kill provider");
    }
}

#[tokio::test]
async fn native_reasoning_round_trips_beside_assistant_tool_calls_and_tool_results() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let base_url = format!("http://{addr}");
    let details = json!([
        {"type":"reasoning.text","text":"inspect","index":0,"future":{"kept":true}},
        {"type":"reasoning.encrypted","data":"sealed","index":1}
    ]);
    let provider_context = json!({
        "provider": "fixture",
        "base_url": base_url,
        "model": "fixture-model",
        "format": "openai-chat-reasoning-v1",
        "artifact": {"reasoning_details": details}
    });
    let history = json!([
        {"role":"user","content":"inspect"},
        {
            "role":"assistant",
            "tool_calls":[{
                "id":"call_1",
                "type":"function",
                "function":{"name":"read_file","arguments":"{\"path\":\"/x\"}"}
            }],
            "provider_context": provider_context
        },
        {"role":"tool","tool_call_id":"call_1","content":"contents"}
    ]);

    let expected_details = details.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request_json(&mut stream).await;
        assert_eq!(
            request["messages"][0],
            json!({"role":"user","content":"inspect"})
        );
        assert_eq!(
            request["messages"][1],
            json!({
                "role":"assistant",
                "tool_calls":[{
                    "id":"call_1",
                    "type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"/x\"}"}
                }],
                "reasoning_details": expected_details
            })
        );
        assert_eq!(
            request["messages"][2],
            json!({"role":"tool","tool_call_id":"call_1","content":"contents"})
        );
        let events = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"next\",\"index\":0,\"unknown\":7}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", events.len(), events);
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });

    let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
    send_completion_messages(&mut stdin, "continuation", history).await;
    let completed = loop {
        let event = next_completion_event(&mut stdout, "continuation").await;
        if event["event"] == "completed" {
            break event;
        }
    };
    assert_eq!(
        completed["provider_context"],
        json!({
            "provider":"fixture",
            "base_url":base_url,
            "model":"fixture-model",
            "format":"openai-chat-reasoning-v1",
            "artifact":{"reasoning_details":[
                {"type":"reasoning.text","text":"next","index":0,"unknown":7}
            ]}
        })
    );
    server.await.expect("server");
    child.kill().await.expect("kill provider");
}

#[tokio::test]
async fn streamed_reasoning_blocks_survive_the_next_completion_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let expected_details = json!([
        {
            "type":"reasoning.text", "text":"The repo\ncontains evidence.",
            "index":0, "id":"text-a", "format":"provider-v1",
            "signature":"signed-a", "future":{"kept":true}
        },
        {"type":"reasoning.text","text":"A separate block.","index":0,"id":"text-b"},
        {"type":"reasoning.summary","summary":"A summary.\n","index":0,"id":"summary-a"},
        {"type":"reasoning.encrypted","data":"sealed-a","index":0,"unknown":[1,2]},
        {"type":"reasoning.encrypted","data":"sealed-a","index":0,"unknown":[1,2]},
        {"type":"future.reasoning","text":"opaque text","future":{"nested":"kept"}},
        {"type":"reasoning.text","text":"After opaque blocks.","index":0,"id":"text-a"}
    ]);
    let expected_request_details = expected_details.clone();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("accept first completion");
        let request = read_request_json(&mut first).await;
        assert_eq!(request["messages"].as_array().expect("messages").len(), 1);
        let deltas = [
            json!({
                "reasoning":"The", "reasoning_content":"The",
                "reasoning_details":[{
                    "type":"reasoning.text","text":"The","index":0,"id":"text-a",
                    "format":"provider-v1","signature":null,"future":{"kept":true}
                }]
            }),
            json!({
                "reasoning":" repo\n", "reasoning_content":" repo\n",
                "reasoning_details":[{"type":"reasoning.text","text":" repo\n","index":0}]
            }),
            json!({
                "reasoning":"contains evidence.", "reasoning_content":"contains evidence.",
                "reasoning_details":[{"type":"reasoning.text","text":"contains evidence.","index":0}]
            }),
            json!({"reasoning_details":[{"type":"reasoning.text","signature":"signed-a","index":0}]}),
            json!({"reasoning_details":[{"type":"reasoning.text","text":"A separate block.","index":0,"id":"text-b"}]}),
            json!({"reasoning_details":[{"type":"reasoning.summary","summary":"A", "index":0,"id":"summary-a"}]}),
            json!({"reasoning_details":[{"type":"reasoning.summary","summary":" summary.\n","index":0}]}),
            json!({"reasoning_details":[
                {"type":"reasoning.encrypted","data":"sealed-a","index":0,"unknown":[1,2]},
                {"type":"reasoning.encrypted","data":"sealed-a","index":0,"unknown":[1,2]},
                {"type":"future.reasoning","text":"opaque text","future":{"nested":"kept"}}
            ]}),
            json!({"reasoning_details":[{"type":"reasoning.text","text":"After opaque blocks.","index":0,"id":"text-a"}]}),
        ];
        let mut events = deltas
            .into_iter()
            .map(|delta| format!("data: {}\n\n", json!({"choices":[{"delta":delta}]})))
            .collect::<String>();
        events.push_str(&format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{
                "delta":{"tool_calls":[{
                    "index":0,"id":"call_1","type":"function",
                    "function":{"name":"read_file","arguments":"{}"}
                }]},
                "finish_reason":"tool_calls"
            }]})
        ));
        first
            .write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(), events
            ).as_bytes())
            .await
            .expect("first response");
        drop(first);

        let (mut second, _) = listener.accept().await.expect("accept replay completion");
        let request = read_request_json(&mut second).await;
        let assistant = &request["messages"][1];
        assert_eq!(assistant["reasoning_details"], expected_request_details);
        assert!(assistant.get("reasoning").is_none());
        assert!(assistant.get("reasoning_content").is_none());
        assert_eq!(request["messages"][2]["tool_call_id"], "call_1");
        let events = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
        );
        second
            .write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(), events
            ).as_bytes())
            .await
            .expect("second response");
    });
    let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
    let initial = json!({"role":"user","content":"inspect the repository"});
    send_completion_messages(&mut stdin, "first", json!([initial])).await;
    let mut visible_reasoning = String::new();
    let completed = loop {
        let event = next_completion_event(&mut stdout, "first").await;
        if event["event"] == "reasoning_delta" {
            visible_reasoning.push_str(event["text"].as_str().expect("reasoning delta"));
        }
        if event["event"] == "completed" {
            break event;
        }
    };
    assert_eq!(visible_reasoning, "The repo\ncontains evidence.");
    assert_eq!(completed["reasoning"], visible_reasoning);
    assert_eq!(
        completed["provider_context"]["artifact"],
        json!({"reasoning_details":expected_details})
    );
    send_completion_messages(&mut stdin, "replay", json!([
        initial,
        {
            "role":"assistant",
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}],
            "provider_context":completed["provider_context"]
        },
        {"role":"tool","tool_call_id":"call_1","content":"repository evidence"}
    ])).await;
    loop {
        let event = next_completion_event(&mut stdout, "replay").await;
        if event["event"] == "completed" {
            assert_eq!(event["text"], "done");
            break;
        }
    }
    server.await.expect("server");
    child.kill().await.expect("kill provider");
}

#[tokio::test]
async fn plaintext_reasoning_fields_round_trip_without_visible_text_substitution() {
    for field in ["reasoning_content", "reasoning"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let base_url = format!("http://{addr}");
        let history = json!([
            {"role":"user","content":"inspect"},
            {
                "role":"assistant",
                "content":"I will inspect it.",
                "tool_calls":[{
                    "id":"call_1",
                    "type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"/x\"}"}
                }],
                "provider_context":{
                    "provider":"fixture",
                    "base_url":base_url,
                    "model":"fixture-model",
                    "format":"openai-chat-reasoning-v1",
                    "artifact":{(field):"private thought"}
                }
            },
            {"role":"tool","tool_call_id":"call_1","content":"contents"}
        ]);

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_request_json(&mut stream).await;
            assert_eq!(
                request["messages"],
                json!([
                    {"role":"user","content":"inspect"},
                    {
                        "role":"assistant",
                        "content":"I will inspect it.",
                        "tool_calls":[{
                            "id":"call_1",
                            "type":"function",
                            "function":{"name":"read_file","arguments":"{\"path\":\"/x\"}"}
                        }],
                        (field):"private thought"
                    },
                    {"role":"tool","tool_call_id":"call_1","content":"contents"}
                ])
            );
            let events = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{(field):"next thought"}}]}),
                json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}),
            );
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", events.len(), events);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });

        let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
        send_completion_messages(&mut stdin, "plaintext-continuation", history).await;
        let completed = loop {
            let event = next_completion_event(&mut stdout, "plaintext-continuation").await;
            if event["event"] == "completed" {
                break event;
            }
        };
        assert_eq!(completed["text"], "done");
        assert_eq!(completed["reasoning"], "next thought");
        assert_eq!(
            completed["provider_context"],
            json!({
                "provider":"fixture",
                "base_url":base_url,
                "model":"fixture-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{(field):"next thought"}
            })
        );
        server.await.expect("server");
        child.kill().await.expect("kill provider");
    }
}

#[tokio::test]
async fn incompatible_context_is_ignored_and_malformed_compatible_context_fails() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request_json(&mut stream).await;
        for index in 1..=5 {
            assert!(request["messages"][index]
                .get("reasoning_details")
                .is_none());
        }
        let events = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", events.len(), events);
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });
    let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;
    send_completion_messages(
        &mut stdin,
        "foreign",
        json!([
            {"role":"user","content":"go"},
            {"role":"assistant","content":"foreign provider","provider_context":{
                "provider":"other","base_url":base_url,"model":"fixture-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":[{"type":"opaque"}]}
            }},
            {"role":"assistant","content":"missing model","provider_context":{
                "provider":"fixture","base_url":base_url,
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":[{"type":"opaque"}]}
            }},
            {"role":"assistant","content":"different model","provider_context":{
                "provider":"fixture","base_url":base_url,"model":"other-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":[{"type":"opaque"}]}
            }},
            {"role":"assistant","content":"different endpoint","provider_context":{
                "provider":"fixture","base_url":"https://other.invalid","model":"fixture-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":[{"type":"opaque"}]}
            }},
            {"role":"assistant","content":"missing endpoint","provider_context":{
                "provider":"fixture","model":"fixture-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":[{"type":"opaque"}]}
            }}
        ]),
    )
    .await;
    loop {
        if next_completion_event(&mut stdout, "foreign").await["event"] == "completed" {
            break;
        }
    }
    server.await.expect("server");

    send_completion_messages(
        &mut stdin,
        "malformed",
        json!([
            {"role":"user","content":"go"},
            {"role":"assistant","content":"prior","provider_context":{
                "provider":"fixture","base_url":base_url,"model":"fixture-model",
                "format":"openai-chat-reasoning-v1",
                "artifact":{"reasoning_details":["bad"]}
            }}
        ]),
    )
    .await;
    let error = next_completion_event(&mut stdout, "malformed").await;
    assert_eq!(error["event"], "error");
    assert!(error["message"]
        .as_str()
        .is_some_and(|message| message.contains("entries must be objects")));
    child.kill().await.expect("kill provider");
}

#[tokio::test]
async fn invalid_or_colliding_request_additions_fail_without_reaching_upstream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (mut child, mut stdin, mut stdout) = spawn_provider(&format!("http://{addr}")).await;

    send_completion(&mut stdin, "non-object", json!(["not", "an", "object"])).await;
    let invalid = next_completion_event(&mut stdout, "non-object").await;
    assert_eq!(invalid["event"], "error");
    assert!(invalid["message"]
        .as_str()
        .is_some_and(|message| message.contains("must be an object")));

    send_completion(&mut stdin, "collision", json!({"model": "replacement"})).await;
    let collision = next_completion_event(&mut stdout, "collision").await;
    assert_eq!(collision["event"], "error");
    assert!(collision["message"]
        .as_str()
        .is_some_and(|message| message.contains("collides with canonical field `model`")));

    assert!(
        timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "invalid additions must fail before HTTP dispatch"
    );
    child.kill().await.expect("kill provider");
}
