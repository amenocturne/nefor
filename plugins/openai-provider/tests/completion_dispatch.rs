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
