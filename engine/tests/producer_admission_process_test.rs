// Real gate and worktree processes preserve correlation after output rejection.
mod producer_admission {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{channel, Receiver};
    use std::time::Duration;

    fn wire(body: Value) -> Value {
        let kind = if matches!(body["kind"].as_str(), Some("ready_ok" | "shutdown")) {
            "system"
        } else {
            "event"
        };
        json!({"type": kind, "from": "engine", "ts": "2026-04-21T00:00:00.000Z", "body": body})
    }

    struct Plugin {
        child: Child,
        input: ChildStdin,
        output: Receiver<Value>,
    }

    impl Plugin {
        fn start(name: &str, args: &[&str]) -> Self {
            let binary = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(name);
            let mut child = Command::new(&binary)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|error| panic!("prepare {} first: {error}", binary.display()));
            let input = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (send, output) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if send.send(serde_json::from_str(&line).unwrap()).is_err() {
                        break;
                    }
                }
            });
            let mut plugin = Self {
                child,
                input,
                output,
            };
            assert_eq!(plugin.recv()["body"]["kind"], "ready");
            plugin.send(json!({"kind": "ready_ok", "engine_version": "test"}));
            plugin.recv(); // hello
            if name == "tool-gate" {
                plugin.recv();
            } // gate mode; capability providers do not advertise
            plugin
        }

        fn send(&mut self, body: Value) {
            writeln!(self.input, "{}", wire(body)).unwrap();
            self.input.flush().unwrap();
        }

        fn recv(&self) -> Value {
            self.output.recv_timeout(Duration::from_secs(15)).unwrap()
        }

        fn finish(&mut self, goodbye: &str) {
            self.send(json!({"kind": "shutdown", "reason": "test"}));
            assert_eq!(self.recv()["body"]["kind"], goodbye);
            assert!(self.child.wait().unwrap().success());
        }
    }

    impl Drop for Plugin {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn real_gate_rejected_forward_settles_outer_then_accepts_later_tool() {
        let mut plugin = Plugin::start("tool-gate", &[]);
        plugin.send(json!({"kind": "tool-gate.tools.advertise", "source": "source-long-name-for-admission-testing-with-extra-routing-overhead-that-must-exceed-the-inbound-envelope-stamp", "tools": [{
            "name": "test", "description": "", "parameters": {},
            "display": {"compact": {"label": "Test"}, "expanded": {"label": "Test", "fields": []}, "result": {"kind": "content", "fields": []}}
        }]}));
        assert_eq!(plugin.recv()["body"]["kind"], "tool.register");
        let mut invoke = json!({"kind": "tool-gate.tool.invoke", "id": "outer", "name": "test", "args": "", "approval_policy": "Default"});
        let overhead = serde_json::to_string(&wire(invoke.clone())).unwrap().len() + 1;
        invoke["args"] = Value::String("x".repeat(16 * 1024 * 1024 - overhead - 4));
        plugin.send(invoke);
        let failure = plugin.recv();
        assert_eq!(failure["body"]["kind"], "tool.result");
        assert_eq!(failure["body"]["id"], "outer");
        assert!(failure["body"]["error"]
            .as_str()
            .unwrap()
            .contains("admission"));
        plugin.send(
            json!({"kind": "tool-gate.tool.invoke", "id": "later", "name": "test", "args": {}, "approval_policy": "Default"}),
        );
        let prepare = plugin.recv();
        assert!(prepare["body"]["kind"]
            .as_str()
            .unwrap()
            .ends_with(".tool.prepare"));
        plugin.send(
            json!({"kind": "tool.prepared", "id": prepare["body"]["id"], "decision": "safe"}),
        );
        let forward = plugin.recv();
        assert_eq!(forward["body"]["caller_id"], "later");
        plugin.send(json!({"kind": "tool.result", "id": forward["body"]["id"], "output": "done"}));
        assert_eq!(plugin.recv()["body"]["id"], "later");
        plugin.finish("tool-gate.goodbye");
    }

    #[test]
    fn real_worktree_rejected_output_settles_actual_id_then_accepts_later_tool() {
        let mut plugin = Plugin::start("git-worktree", &[]);
        let mut invoke = json!({"kind": "git-worktree.capability.invoke", "id": "actual", "capability": "", "args": {}});
        let overhead = serde_json::to_string(&wire(invoke.clone())).unwrap().len() + 1;
        invoke["capability"] = Value::String("x".repeat(16 * 1024 * 1024 - overhead - 4));
        plugin.send(invoke);
        let failure = plugin.recv();
        assert_eq!(failure["body"]["id"], "actual");
        assert!(failure["body"]["error"]
            .as_str()
            .unwrap()
            .contains("Git effects"));
        assert!(failure["body"].get("output").is_none());
        plugin.send(json!({"kind": "git-worktree.capability.invoke", "id": "later", "capability": "git_worktree_create", "args": {}}));
        assert_eq!(plugin.recv()["body"]["id"], "later");
        plugin.finish("git-worktree.goodbye");
    }
}
