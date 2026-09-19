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
                    "argv": ["/bin/cat"], "cwd": "/", "timeout": unbounded(), "stdin": "hello"
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
            async fn streams_stdout_and_stderr_independently() {
                let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
                let _ = run_cancellable_streaming(
                    &json!({ "argv": ["/bin/sh", "-c", "printf out; printf err >&2"], "cwd": "/", "timeout": unbounded() }),
                    None,
                    Some(stream_tx),
                ).await.unwrap();
                let chunks = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
                assert!(chunks
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::Stdout(bytes) if bytes == b"out")));
                assert!(chunks
                    .iter()
                    .any(|chunk| matches!(chunk, StreamChunk::Stderr(bytes) if bytes == b"err")));
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
            async fn worst_case_escaped_output_on_both_streams_fits_one_ncp_frame() {
                // \x01 escapes to six JSON bytes (), the largest per-byte growth.
                let result = run(&json!({
                    "argv": ["/bin/sh", "-c", "head -c 20971520 /dev/zero | tr '\\0' '\\001' | tee /dev/stderr"],
                    "cwd": "/",
                    "timeout": unbounded()
                }))
                .await
                .unwrap();
                let frame = serde_json::to_string(&result).unwrap();
                assert!(frame.len() < 16 * 1024 * 1024, "frame is {} bytes", frame.len());
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
    async fn spawned_invocations_run_concurrently() {
        let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let slow = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "slow-call",
            "name": "shell.script",
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
            "kind": "basic-tools.tool.invoke",
            "id": "fast-call",
            "name": "shell.script",
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
        let slow_task = spawn_tool_invoke(&out_tx, &slow, &cancels);
        let fast_task = spawn_tool_invoke(&out_tx, &fast, &cancels);

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
        slow_task.await.unwrap();
        fast_task.await.unwrap();
    }

    async fn recv_result_body(rx: &mut mpsc::Receiver<PluginOutgoing>) -> Map<String, Value> {
        loop {
            let msg = timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for tool.result")
                .expect("tool.result message");
            match msg.body {
                Body::Event(body)
                    if body.get("kind").and_then(Value::as_str) == Some("tool.result") =>
                {
                    return body;
                }
                Body::Event(_) => {}
                Body::System(other) => panic!("expected event body, got {other:?}"),
            }
        }
    }

    // Events with a different kind (e.g. another plugin's broadcast) are
    // ignored — no spurious replies.
}
