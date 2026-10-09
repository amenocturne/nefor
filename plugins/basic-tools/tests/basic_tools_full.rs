mod error {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/error.rs"));
}

mod tools {

    pub mod process {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/process.rs"));
    }

    pub mod process_exec {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/process_exec.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;
            use serde_json::json;

            fn unbounded() -> Value {
                json!({ "present": false, "milliseconds": 0 })
            }

            #[tokio::test]
            async fn preserves_argument_boundaries_and_nonzero_status() {
                let result = run(&json!({
                    "argv": ["/bin/sh", "-c", "printf '%s' \"$1\"; printf err >&2; exit 7", "sh", "a b"],
                    "cwd": "/",
                    "timeout": unbounded()
                }))
                .await
                .unwrap();
                assert_eq!(result["stdout"], "a b");
                assert_eq!(result["stderr"], "err");
                assert_eq!(result["termination"], json!({ "kind": "code", "code": 7 }));
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn reports_signal_termination() {
                let result = run(&json!({
                    "argv": ["/bin/sh", "-c", "kill -TERM $$"],
                    "cwd": "/",
                    "timeout": unbounded()
                }))
                .await
                .unwrap();
                assert_eq!(
                    result["termination"],
                    json!({ "kind": "signal", "signal": libc::SIGTERM })
                );
            }

            #[tokio::test]
            async fn forwards_stdin() {
                let result = run(&json!({
                    "argv": ["cat"], "cwd": "/", "timeout": unbounded(), "stdin": "hello"
                }))
                .await
                .unwrap();
                assert_eq!(result["stdout"], "hello");
            }

            #[tokio::test]
            async fn timeout_returns_partial_streams() {
                let error = run(&json!({
                    "argv": ["/bin/sh", "-c", "printf before; printf warning >&2; sleep 5"],
                    "cwd": "/",
                    "timeout": { "present": true, "milliseconds": 100 }
                }))
                .await
                .unwrap_err();
                assert!(
                    matches!(error, ToolError::ProcessTimeout { stdout, stderr, .. } if stdout == "before" && stderr == "warning")
                );
            }

            #[tokio::test]
            async fn cancellation_kills_process_group_and_returns_partial_streams() {
                let directory = tempfile::tempdir().unwrap();
                let marker = directory.path().join("survived");
                let script = format!("printf started; sleep 1; touch {}", marker.display());
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let invocation = tokio::spawn(async move {
                    run_cancellable_streaming(
                        &json!({ "argv": ["/bin/sh", "-c", script], "cwd": "/", "timeout": unbounded() }),
                        Some(cancel_rx),
                        None,
                    )
                    .await
                });
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                cancel_tx.send(()).unwrap();
                let error = invocation.await.unwrap().unwrap_err();
                assert!(
                    matches!(error, ToolError::ProcessCancelled { stdout, stderr } if stdout == "started" && stderr.is_empty())
                );
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                assert!(!marker.exists(), "a cancelled grandchild must not survive");
            }

            #[tokio::test]
            async fn cancellation_retains_group_authority_after_leader_exits() {
                let directory = tempfile::tempdir().unwrap();
                let pid_file = directory.path().join("leader");
                let script = format!(
                    "echo $$ > {}; printf started; sleep 30 & exit 0",
                    pid_file.display()
                );
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let invocation = tokio::spawn(async move {
                    run_cancellable_streaming(
                        &json!({ "argv": ["/bin/sh", "-c", script], "cwd": "/", "timeout": unbounded() }),
                        Some(cancel_rx), None,
                    ).await
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                            let pid = pid.trim().parse::<libc::pid_t>().unwrap();
                            if unsafe { libc::kill(pid, 0) } == -1
                                && std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::ESRCH)
                            {
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    !invocation.is_finished(),
                    "descendant still holds stdout/stderr"
                );
                cancel_tx.send(()).unwrap();
                let error = tokio::time::timeout(std::time::Duration::from_secs(5), invocation)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert!(
                    matches!(error, ToolError::ProcessCancelled { stdout, stderr }
                    if stdout == "started" && stderr.is_empty())
                );
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn timeout_includes_descendant_pipes_after_leader_exit() {
                let directory = tempfile::tempdir().unwrap();
                let descendant = directory.path().join("descendant");
                let script = format!(
                    "printf started; sleep 30 & echo $! > {}; exit 0",
                    descendant.display()
                );
                let error = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    run(&json!({
                        "argv": ["/bin/sh", "-c", script], "cwd": "/",
                        "timeout": {"present": true, "milliseconds": 100}
                    })),
                )
                .await
                .expect("deadline survives leader exit")
                .unwrap_err();
                assert!(
                    matches!(error, ToolError::ProcessTimeout {timeout_ms:100, stdout, stderr}
                    if stdout == "started" && stderr.is_empty())
                );
                let pid: i32 = std::fs::read_to_string(descendant)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while unsafe { libc::kill(pid, 0) } != -1 {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("descendant does not survive timeout");
            }

            #[tokio::test]
            async fn streams_stdout_and_stderr_independently() {
                let preview = std::sync::Arc::new(process::LivePreview::default());
                let _ = run_cancellable_streaming(
                    &json!({ "argv": ["/bin/sh", "-c", "printf out; printf err >&2"], "cwd": "/", "timeout": unbounded() }),
                    None,
                    Some(preview.clone()),
                ).await.unwrap();
                assert_eq!(
                    preview.take(),
                    vec![
                        process::StreamChunk::Stdout(process::PreviewChunk {
                            bytes: b"out".to_vec(),
                            skipped_bytes: 0
                        }),
                        process::StreamChunk::Stderr(process::PreviewChunk {
                            bytes: b"err".to_vec(),
                            skipped_bytes: 0
                        }),
                    ]
                );
                assert!(preview.take().is_empty(), "a drain forwards each byte once");
            }

            #[tokio::test]
            async fn undrained_preview_keeps_only_the_latest_window() {
                let preview = std::sync::Arc::new(process::LivePreview::default());
                let _ = run_cancellable_streaming(
                    &json!({ "argv": ["/bin/sh", "-c", "head -c 1048576 /dev/zero | tr '\\0' a; printf END"], "cwd": "/", "timeout": unbounded() }),
                    None,
                    Some(preview.clone()),
                ).await.unwrap();
                let chunks = preview.take();
                let [process::StreamChunk::Stdout(bytes)] = chunks.as_slice() else {
                    panic!("expected one stdout chunk, got {chunks:?}");
                };
                let text = String::from_utf8(bytes.bytes.clone()).unwrap();
                let skipped = 1_048_576 + 3 - process::PREVIEW_WINDOW_BYTES;
                assert_eq!(bytes.skipped_bytes, skipped as u64);
                assert!(text.ends_with("aaaEND"));
                assert!(text.len() < process::PREVIEW_WINDOW_BYTES + 64);
            }

            #[tokio::test]
            async fn oversized_output_keeps_head_and_tail_within_bound() {
                let result = run(&json!({
                    "argv": ["/bin/sh", "-c", "printf START; head -c 3145728 /dev/zero | tr '\\0' a; printf END"],
                    "cwd": "/",
                    "timeout": unbounded()
                }))
                .await
                .unwrap();
                let stdout = result["stdout"].as_str().unwrap();
                let omitted = 5 + 3_145_728 + 3 - 2 * process::RETAINED_EDGE_BYTES;
                assert!(stdout.starts_with("STARTaaa"));
                assert!(stdout.ends_with("aaaEND"));
                assert!(stdout.contains(&format!("[... {omitted} bytes of output omitted ...]")));
                assert!(stdout.len() < 2 * process::RETAINED_EDGE_BYTES + 64);
            }

            #[tokio::test]
            async fn worst_case_escaped_retained_payload_has_bounded_inner_json() {
                // This checks only the inner payload; full-envelope admission also
                // includes the real correlation ID, error rendering and LF.
                // \x01 escapes to six JSON bytes (`\u0001`).
                let result = run(&json!({
                    "argv": ["/bin/sh", "-c", "head -c 20971520 /dev/zero | tr '\\0' '\\001' | tee /dev/stderr"],
                    "cwd": "/",
                    "timeout": unbounded()
                }))
                .await
                .unwrap();
                let frame = serde_json::to_string(&result).unwrap();
                assert!(
                    frame.len() < 16 * 1024 * 1024,
                    "frame is {} bytes",
                    frame.len()
                );
            }
        }
    }

    pub mod read_file {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/read_file.rs"
        ));
    }

    pub mod read_image {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/read_image.rs"
        ));
    }

    pub mod shell_script {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/shell_script.rs"
        ));

        #[cfg(test)]
        mod tests {
            use super::*;
            use serde_json::json;

            #[tokio::test]
            async fn lowers_to_shell_and_returns_structured_result() {
                let result = run(&json!({
                    "script": "printf out; printf err >&2",
                    "cwd": "/",
                    "timeout": { "present": false, "milliseconds": 0 }
                }))
                .await
                .unwrap();
                assert_eq!(result["stdout"], "out");
                assert_eq!(result["stderr"], "err");
                assert_eq!(result["termination"], json!({ "kind": "code", "code": 0 }));
            }
        }
    }

    pub mod write_file {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tools/write_file.rs"
        ));
    }

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/runtime.rs"));
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::timeout;
    #[tokio::test]
    async fn forwarding_flushes_invalid_suffixes_independently_before_terminal_result() {
        let (tx, mut rx) = mpsc::channel(32);
        let body = json!({"id":"suffix", "name":"shell.script", "args":{
            "script": "printf '\\320\\226\\342\\202'; printf '\\360\\237\\230\\200\\320' >&2",
            "cwd":".", "timeout":{"present":false,"milliseconds":0}
        }})
        .as_object()
        .unwrap()
        .clone();
        let task = tokio::spawn(async move { handle_tool_invoke(&tx, &body, None).await });
        let mut stdout = String::new();
        let mut stderr = String::new();
        let result = loop {
            let message = timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Body::Event(body) = message.body {
                match body["kind"].as_str().unwrap() {
                    "tool.stream" => {
                        let text = body["text"].as_str().unwrap();
                        match body["stream"].as_str().unwrap() {
                            "stdout" => stdout.push_str(text),
                            "stderr" => stderr.push_str(text),
                            other => panic!("unexpected stream {other}"),
                        }
                    }
                    "tool.result" => break body,
                    other => panic!("unexpected message {other}"),
                }
            }
        };
        task.await.unwrap().unwrap();
        assert_eq!(stdout, "Ж�");
        assert_eq!(stderr, "😀�");
        assert_eq!(result["output"]["stdout"], stdout);
        assert_eq!(result["output"]["stderr"], stderr);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn spawned_invocations_run_concurrently() {
        let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let slow = json!({
            "kind": "basic-tools.capability.invoke",
            "id": "slow-call",
            "capability": "shell.script",
            "args": {
                "script": "sleep 0.2; echo SLOW_DONE",
                "cwd": ".",
                "timeout": { "present": false, "milliseconds": 0 }
            }
        })
        .as_object()
        .expect("obj")
        .clone();
        let fast = json!({
            "kind": "basic-tools.capability.invoke",
            "id": "fast-call",
            "capability": "shell.script",
            "args": {
                "script": "echo FAST_DONE",
                "cwd": ".",
                "timeout": { "present": false, "milliseconds": 0 }
            }
        })
        .as_object()
        .expect("obj")
        .clone();

        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let slow_task = spawn_invoke(&out_tx, &slow, &cancels);
        let fast_task = spawn_invoke(&out_tx, &fast, &cancels);

        let first = recv_result_body(&mut out_rx).await;
        let second = recv_result_body(&mut out_rx).await;

        assert_eq!(
            first.get("id").and_then(Value::as_str),
            Some("fast-call"),
            "fast invocation should not wait for the slow invocation"
        );
        assert!(first
            .get("output")
            .and_then(|output| output.get("stdout"))
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("FAST_DONE")));
        assert_eq!(second.get("id").and_then(Value::as_str), Some("slow-call"));
        assert!(second
            .get("output")
            .and_then(|output| output.get("stdout"))
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("SLOW_DONE")));
        slow_task.await.unwrap().unwrap();
        fast_task.await.unwrap().unwrap();
    }

    // Logcat-scale output must reach the bus as a paced preview, not one event
    // per pipe read.
    #[tokio::test]
    async fn flooding_process_streams_at_the_flush_cadence() {
        let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(8);
        let flood = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "flood",
            "name": "shell.script",
            "args": {
                "script": "head -c 536870912 /dev/zero | tr '\\0' a | tee /dev/stderr; printf END",
                "cwd": ".",
                "timeout": { "present": false, "milliseconds": 0 }
            }
        })
        .as_object()
        .expect("obj")
        .clone();

        let started = std::time::Instant::now();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&out_tx, &flood, &cancels);
        let mut stream_events = 0_u128;
        let mut last_stdout = String::new();
        let result = loop {
            let msg = timeout(Duration::from_secs(60), out_rx.recv())
                .await
                .expect("timed out waiting for tool.result")
                .expect("event");
            let Body::Event(body) = msg.body else {
                continue;
            };
            match body.get("kind").and_then(Value::as_str) {
                Some("tool.stream") => {
                    stream_events += 1;
                    if body.get("stream").and_then(Value::as_str) == Some("stdout") {
                        last_stdout = body["text"].as_str().expect("text").to_owned();
                    }
                }
                Some("tool.result") => break body,
                _ => {}
            }
        };
        let elapsed = started.elapsed().as_millis();
        task.await.unwrap().unwrap();

        let ticks = elapsed / PREVIEW_FLUSH_INTERVAL.as_millis() + 2;
        assert!(
            stream_events <= 2 * ticks,
            "{stream_events} stream events in {elapsed} ms exceeds two per tick"
        );
        assert!(
            last_stdout.ends_with("aaaEND"),
            "final flush carries the tail"
        );
        assert!(result["output"]["stdout"]
            .as_str()
            .is_some_and(|s| s.ends_with("aaaEND")));
    }

    async fn recv_result_body(rx: &mut mpsc::Receiver<PluginOutgoing>) -> Map<String, Value> {
        loop {
            let msg = timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for capability.result")
                .expect("capability.result message");
            match msg.body {
                Body::Event(body)
                    if body.get("kind").and_then(Value::as_str) == Some("capability.result") =>
                {
                    return body;
                }
                Body::Event(_) => {}
                Body::System(other) => panic!("expected event body, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod producer_failure_tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{timeout, Duration};

    fn request(id: &str, script: &str) -> Map<String, Value> {
        json!({"kind": "basic-tools.tool.invoke", "id": id, "name": "shell.script",
            "args": {"script": script, "cwd": "/", "timeout": {"present": false, "milliseconds": 0}}})
            .as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn actual_giant_process_result_is_bounded_marked_success_and_accepts_later_work() {
        let (out, mut incoming) = mpsc::channel(32);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let body = request("giant", "head -c 17825792 /dev/zero | tr '\\000' x");
        let task = spawn_tool_invoke(&out, &body, &cancels);
        let mut streamed = 0;
        let terminal = timeout(Duration::from_secs(20), async {
            loop {
                let message = incoming.recv().await.unwrap();
                if let Body::Event(body) = message.body {
                    if body["kind"] == "tool.result" {
                        break body;
                    }
                    streamed += body["text"].as_str().unwrap().len();
                }
            }
        })
        .await
        .unwrap();
        task.await.unwrap().unwrap();
        assert!(streamed > 0 && streamed < 17825792);
        assert_eq!(terminal["id"], "giant");
        assert!(terminal.get("error").is_none());
        let stdout = terminal["output"]["stdout"].as_str().unwrap();
        assert!(stdout.contains("16777216 bytes of output omitted"));
        assert!(stdout.len() < 1024 * 1024 + 100);
        assert_eq!(terminal["output"]["termination"]["code"], 0);
        assert!(cancels.lock().unwrap().is_empty());
        let later = spawn_tool_invoke(&out, &request("later", "printf ok"), &cancels);
        let terminal = timeout(Duration::from_secs(2), async {
            loop {
                if let Body::Event(body) = incoming.recv().await.unwrap().body {
                    if body["kind"] == "tool.result" {
                        break body;
                    }
                }
            }
        })
        .await
        .unwrap();
        later.await.unwrap().unwrap();
        assert_eq!(terminal["id"], "later");
        assert_eq!(terminal["output"]["stdout"], "ok");
        assert!(incoming.try_recv().is_err());
        assert!(cancels.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejected_stream_promptly_kills_and_reaps_active_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let id = "x".repeat(16 * 1024 * 1024 - 1000);
        let body = request(
            &id,
            &format!(
                "echo $$ > {}; head -c 8192 /dev/zero; sleep 30",
                pid_path.display()
            ),
        );
        // The invocation itself fits; the required stream frame does not.
        let (admission, mut admitted) = mpsc::channel(1);
        checked_send(&admission, PluginOutgoing::event(body.clone()))
            .await
            .unwrap();
        assert!(admitted.recv().await.is_some());
        let (out, mut incoming) = mpsc::channel(4);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&out, &body, &cancels);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let terminal = incoming.recv().await.unwrap();
        let Body::Event(terminal) = terminal.body else {
            panic!("event")
        };
        assert_eq!(terminal["id"], id);
        assert!(terminal["error"]
            .as_str()
            .unwrap()
            .contains("frame_too_large"));
        assert!(incoming.try_recv().is_err());
        assert!(cancels.lock().unwrap().is_empty());
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "leader must be reaped before terminal"
        );
    }

    #[tokio::test]
    async fn detached_transport_error_supervises_and_reaps_active_sibling() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let active = request(
            "sibling",
            &format!("echo $$ > {}; exec sleep 30", pid_path.display()),
        );
        let (out, _incoming) = mpsc::channel(4);
        let (input, mut envelopes) = mpsc::channel(4);
        let supervisor = tokio::spawn(async move { run_dispatch_loop(&out, &mut envelopes).await });
        let envelope =
            |body: Map<String, Value>| {
                Envelope::parse_line(&serde_json::json!({
            "type": "event", "from": "engine", "ts": "2026-01-01T00:00:00Z", "body": body
        }).to_string()).unwrap()
            };
        input.send(Ok(envelope(active))).await.unwrap();
        timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let failed = serde_json::json!({"kind": "basic-tools.tool.invoke", "id": "x".repeat(16 * 1024 * 1024),
            "name": "process.exec", "args": {"argv": []}}).as_object().unwrap().clone();
        input.send(Ok(envelope(failed))).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), supervisor)
                .await
                .unwrap()
                .unwrap(),
            Err(TransportError::FrameTooLarge { .. })
        ));
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[tokio::test]
    async fn closed_writer_while_process_active_cancels_reaps_and_clears_registry() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let body = request(
            "active",
            &format!("echo $$ > {}; exec sleep 30", pid_path.display()),
        );
        let (out, incoming) = mpsc::channel(4);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&out, &body, &cancels);
        timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(incoming);
        assert!(matches!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            Err(TransportError::WriterClosed)
        ));
        assert!(cancels.lock().unwrap().is_empty());
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    #[tokio::test]
    async fn closed_writer_during_shell_descendant_spawn_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let body = request(
            "active",
            &format!("echo $$ > {}; sleep 30", pid_path.display()),
        );
        let (out, incoming) = mpsc::channel(4);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&out, &body, &cancels);
        timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(incoming);
        assert!(matches!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            Err(TransportError::WriterClosed)
        ));
        assert!(cancels.lock().unwrap().is_empty());
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
    #[tokio::test]
    async fn closed_writer_reclaims_coordinated_descendant_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let descendant = directory.path().join("descendant");
        let body = request(
            "active",
            &format!(
                "sleep 30 & echo $! > {}; echo $$ > {}; wait",
                descendant.display(),
                pid_path.display()
            ),
        );
        let (out, incoming) = mpsc::channel(4);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&out, &body, &cancels);
        timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(incoming);
        assert!(matches!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            Err(TransportError::WriterClosed)
        ));
        assert!(cancels.lock().unwrap().is_empty());
        let pid: i32 = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let child: i32 = std::fs::read_to_string(descendant)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        timeout(Duration::from_secs(5), async {
            while unsafe { libc::kill(child, 0) } != -1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant reclaimed, not only leader");
    }
}

#[cfg(test)]
mod runtime_boundary_contract_tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn actual_id_escape_and_diagnostic_envelopes_reject_once_without_enqueueing_oversize() {
        let id = "x".repeat(5 * 1024 * 1024);
        let text = "\u{1}".repeat(1024 * 1024);
        let outcomes = [
            Ok(json!({"stdout": text, "stderr": text})),
            Err(ToolError::ProcessTimeout {
                timeout_ms: 1,
                stdout: text.clone(),
                stderr: text.clone(),
            }),
            Err(ToolError::ProcessCancelled {
                stdout: text.clone(),
                stderr: text.clone(),
            }),
        ];
        for outcome in outcomes {
            let (tx, mut rx) = mpsc::channel(2);
            publish_outcome(&tx, &id, outcome).await.unwrap();
            let row = rx.recv().await.unwrap();
            assert!(row.to_line().len() + 1 <= nefor_plugin_sdk::MAX_FRAME_BYTES);
            let Body::Event(body) = row.body else {
                panic!("event")
            };
            assert_eq!(body["id"], id);
            assert!(body["error"].as_str().unwrap().contains("frame_too_large"));
            assert!(body.get("output").is_none());
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn lf_inclusive_exact_full_envelope_cap_and_unencodable_fallback() {
        let (tx, mut rx) = mpsc::channel(2);
        let overhead = PluginOutgoing::event(tool_result_ok_body("", json!("")))
            .to_line()
            .len()
            + 1;
        let id = "x".repeat(nefor_plugin_sdk::MAX_FRAME_BYTES - overhead);
        publish_outcome(&tx, &id, Ok(json!(""))).await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap().to_line().len() + 1,
            nefor_plugin_sdk::MAX_FRAME_BYTES
        );
        let id = format!("{id}x");
        assert!(matches!(
            publish_outcome(&tx, &id, Ok(json!(""))).await,
            Err(TransportError::FrameTooLarge { .. })
        ));
        assert!(
            rx.try_recv().is_err(),
            "fallback too large must fail supervision"
        );
    }

    #[test]
    fn decoder_gap_resets_suffix_before_marker_and_new_span() {
        let mut decoder = process::StreamDecoder::default();
        assert_eq!(
            decode_preview(
                &mut decoder,
                process::PreviewChunk {
                    bytes: vec![0xe2],
                    skipped_bytes: 0
                }
            ),
            ""
        );
        assert_eq!(
            decode_preview(
                &mut decoder,
                process::PreviewChunk {
                    bytes: vec![0x82, 0xac],
                    skipped_bytes: 7
                }
            ),
            "�[... 7 bytes skipped in live output ...]\n��"
        );
        assert_eq!(decoder.finish(), "");
        assert_eq!(
            decode_preview(
                &mut decoder,
                process::PreviewChunk {
                    bytes: vec![],
                    skipped_bytes: 3
                }
            ),
            "[... 3 bytes skipped in live output ...]\n"
        );
    }

    #[tokio::test]
    async fn blocked_peer_at_timeout_reaps_process_and_returns_transport_failure() {
        let directory = tempfile::tempdir().unwrap();
        let pid = directory.path().join("pid");
        let body = json!({"kind": "basic-tools.tool.invoke", "id": "blocked", "name": "shell.script",
            "args": {"script": format!("echo $$ > {}; printf first; exec sleep 30", pid.display()), "cwd": "/",
                "timeout": {"present": true, "milliseconds": 100}}}).as_object().unwrap().clone();
        let (tx, _rx) = mpsc::channel(1);
        send_event(&tx, tool_result_error_body("occupier", "full"))
            .await
            .unwrap();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let result = timeout(
            Duration::from_secs(3),
            spawn_tool_invoke(&tx, &body, &cancels),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(result, Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        assert!(cancels.lock().unwrap().is_empty());
        let leader: i32 = std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(leader, 0) }, -1);
    }

    #[tokio::test]
    async fn slow_receiver_keeps_loss_metadata_and_final_eof_before_once_only_terminal() {
        let body = json!({"kind": "basic-tools.tool.invoke", "id": "slow", "name": "shell.script",
            "args": {"script": "head -c 2097152 /dev/zero | tr '\\000' x; printf '\\342'; printf '\\360' >&2", "cwd": "/",
                "timeout": {"present": false, "milliseconds": 0}}}).as_object().unwrap().clone();
        let (tx, mut rx) = mpsc::channel(1);
        send_event(&tx, tool_result_error_body("occupier", "full"))
            .await
            .unwrap();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&tx, &body, &cancels);
        tokio::time::sleep(Duration::from_millis(300)).await;
        rx.recv().await.unwrap();
        let mut skipped = 0;
        let mut stdout = String::new();
        let mut stderr = String::new();
        timeout(Duration::from_secs(3), async {
            loop {
                let Body::Event(row) = rx.recv().await.unwrap().body else {
                    panic!("event")
                };
                if row["kind"] == "tool.result" {
                    assert!(row.get("output").is_some());
                    break;
                }
                skipped += row["skipped_bytes"].as_u64().unwrap();
                if row["stream"] == "stdout" {
                    stdout.push_str(row["text"].as_str().unwrap());
                } else {
                    stderr.push_str(row["text"].as_str().unwrap());
                }
            }
        })
        .await
        .unwrap();
        task.await.unwrap().unwrap();
        assert!(skipped > 0);
        assert!(stdout.ends_with('�'));
        assert_eq!(stderr, "�");
        assert!(rx.try_recv().is_err());
        assert!(cancels.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod runtime_cancellation_and_real_frame_tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{timeout, Duration};

    fn request(id: &str, script: &str) -> Map<String, Value> {
        json!({"kind": "basic-tools.tool.invoke", "id": id, "name": "shell.script",
            "args": {"script": script, "cwd": "/", "timeout": {"present": false, "milliseconds": 0}}})
            .as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn real_five_mib_id_and_escaped_both_streams_reject_then_later_work_succeeds() {
        let id = "x".repeat(5 * 1024 * 1024);
        let body = request(
            &id,
            "head -c 2097152 /dev/zero | tr '\\000' '\\001' | tee /dev/stderr",
        );
        let (tx, mut rx) = mpsc::channel(1);
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let task = spawn_tool_invoke(&tx, &body, &cancels);
        let terminal = timeout(Duration::from_secs(15), async {
            loop {
                let row = rx.recv().await.unwrap();
                assert!(row.to_line().len() + 1 <= nefor_plugin_sdk::MAX_FRAME_BYTES);
                let Body::Event(body) = row.body else {
                    panic!("event")
                };
                if body["kind"] == "tool.result" {
                    break body;
                }
            }
        })
        .await
        .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(terminal["id"], id);
        assert!(terminal["error"]
            .as_str()
            .unwrap()
            .contains("frame_too_large"));
        assert!(terminal.get("output").is_none());
        assert!(cancels.lock().unwrap().is_empty());
        let later = spawn_tool_invoke(&tx, &request("later", "printf ok"), &cancels);
        timeout(Duration::from_secs(5), async {
            loop {
                let Body::Event(body) = rx.recv().await.unwrap().body else {
                    panic!("event")
                };
                if body["kind"] == "tool.result" {
                    assert_eq!(body["output"]["stdout"], "ok");
                    break;
                }
            }
        })
        .await
        .unwrap();
        later.await.unwrap().unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_final_preview_and_cleans_registry() {
        let directory = tempfile::tempdir().unwrap();
        let completed = directory.path().join("completed");
        let (tx, _rx) = mpsc::channel(1);
        send_event(&tx, tool_result_error_body("occupier", "full"))
            .await
            .unwrap();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let body = request(
            "blocked",
            &format!("printf suffix; touch {}", completed.display()),
        );
        let task = spawn_tool_invoke(&tx, &body, &cancels);
        timeout(Duration::from_secs(5), async {
            while !completed.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle_tool_cancel(&cancels, json!({"id": "blocked"}).as_object().unwrap());
        let result = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::Interrupted)
        );
        assert!(cancels.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_active_preview_after_reap() {
        let directory = tempfile::tempdir().unwrap();
        let pid = directory.path().join("pid");
        let (tx, _rx) = mpsc::channel(1);
        send_event(&tx, tool_result_error_body("occupier", "full"))
            .await
            .unwrap();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let body = request(
            "blocked",
            &format!("echo $$ > {}; printf first; exec sleep 30", pid.display()),
        );
        let task = spawn_tool_invoke(&tx, &body, &cancels);
        timeout(Duration::from_secs(5), async {
            while std::fs::read_to_string(&pid)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        handle_tool_cancel(&cancels, json!({"id": "blocked"}).as_object().unwrap());
        let result = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        assert!(cancels.lock().unwrap().is_empty());
        let leader: i32 = std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(leader, 0) }, -1);
    }
    #[tokio::test]
    async fn cancellation_interrupts_blocked_shell_descendant_preview() {
        let directory = tempfile::tempdir().unwrap();
        let pid = directory.path().join("pid");
        let (tx, _rx) = mpsc::channel(1);
        send_event(&tx, tool_result_error_body("occupier", "full"))
            .await
            .unwrap();
        let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
        let body = request(
            "blocked",
            &format!("echo $$ > {}; printf first; sleep 30", pid.display()),
        );
        let task = spawn_tool_invoke(&tx, &body, &cancels);
        timeout(Duration::from_secs(5), async {
            while std::fs::read_to_string(&pid)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok())
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        handle_tool_cancel(&cancels, json!({"id": "blocked"}).as_object().unwrap());
        let result = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        assert!(cancels.lock().unwrap().is_empty());
        let leader: i32 = std::fs::read_to_string(pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(leader, 0) }, -1);
    }
}
