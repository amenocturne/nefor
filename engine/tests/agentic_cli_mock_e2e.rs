//! Headless starter acceptance: the real engine/plugins, deterministic provider,
//! closed stdin, and a plugin directory in which no TUI executable exists.
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}

fn binaries() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|p| if p.is_absolute() { p } else { root().join(p) })
        .unwrap_or_else(|| root().join("target"))
        .join("debug")
}

#[derive(Clone, Copy)]
enum Intervention<'a> {
    None,
    Sigint {
        started_runs: usize,
    },
    KillPlugin {
        executable: &'a str,
        require_descendant: bool,
    },
}

fn run(dir: &Path, args: &[&str]) -> Output {
    run_inner(dir, args, Intervention::None, &[])
}

fn run_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    run_inner(dir, args, Intervention::None, env)
}

fn processes() -> Vec<(u32, u32, u32, String)> {
    let output = Command::new("/bin/ps")
        .env_clear()
        .args(["-axo", "pid=,ppid=,pgid=,command="])
        .output()
        .expect("list fixture processes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
                line.to_owned(),
            ))
        })
        .collect()
}

fn plugin_process(parent: u32, executable: &str) -> Option<(u32, u32)> {
    processes()
        .into_iter()
        .find_map(|(pid, ppid, pgid, command)| {
            let binary = command
                .split_whitespace()
                .nth(3)
                .and_then(|path| Path::new(path).file_name())
                .and_then(|name| name.to_str());
            (ppid == parent && binary == Some(executable)).then_some((pid, pgid))
        })
}

fn group_is_empty(group: u32) -> bool {
    !processes().into_iter().any(|(_, _, pgid, _)| pgid == group)
}

fn accepted_session_text(dir: &Path, prompt: &str) -> String {
    std::fs::read_dir(dir.join("data/nefor/sessions"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .find(|text| text.contains(prompt))
        .unwrap_or_default()
}

fn run_inner(
    dir: &Path,
    args: &[&str],
    intervention: Intervention<'_>,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(binaries().join("nefor"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NEFOR_RUNTIME_ROOT", root())
        .env("NEFOR_EXECUTABLE_ROOT", dir.join("bin"))
        .env("NEFOR_TEST_FAST_MOCK", "1")
        .env("NEFOR_STARTUP_TIMEOUT_MS", "10000")
        .env("OPENAI_PROVIDER_API_KEY", "offline-sentinel")
        .arg("--config")
        .arg(root().join("examples/nefor-agent"))
        .arg("run")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn headless engine");
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let drain = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let out = drain(Box::new(stdout));
    let err = drain(Box::new(stderr));
    let prompt = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--prompt").then_some(pair[1]))
        .unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut intervened = false;
    let mut killed_process_groups = Vec::new();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if !intervened {
            let persisted = accepted_session_text(dir, prompt);
            let started_runs = persisted.matches("mag.run_started").count();
            let target = match intervention {
                Intervention::None => None,
                Intervention::Sigint {
                    started_runs: required,
                } if started_runs >= required => Some((child.id(), "-INT")),
                Intervention::KillPlugin {
                    executable,
                    require_descendant,
                } if started_runs >= 1 => {
                    plugin_process(child.id(), executable).and_then(|(pid, group)| {
                        let child_groups = processes()
                            .into_iter()
                            .filter_map(|(_, ppid, pgid, _)| (ppid == pid).then_some(pgid))
                            .collect::<Vec<_>>();
                        let same_group_descendant = processes()
                            .into_iter()
                            .any(|(member, _, pgid, _)| pgid == group && member != pid);
                        if require_descendant && child_groups.is_empty() && !same_group_descendant {
                            return None;
                        }
                        killed_process_groups.push(group);
                        killed_process_groups.extend(child_groups);
                        Some((pid, "-TERM"))
                    })
                }
                _ => None,
            };
            if let Some((pid, signal)) = target {
                assert!(Command::new("/bin/kill")
                    .env_clear()
                    .args([signal, &pid.to_string()])
                    .status()
                    .unwrap()
                    .success());
                intervened = true;
            }
        }
        if Instant::now() >= deadline {
            let _ = Command::new("/bin/kill")
                .env_clear()
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "headless request timed out; retained artifacts: {}",
                dir.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    if !matches!(intervention, Intervention::None) {
        assert!(intervened, "the test must intervene after accepted work");
    }
    for group in killed_process_groups {
        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while !group_is_empty(group) && Instant::now() < cleanup_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            group_is_empty(group),
            "runtime left descendants in process group {group}"
        );
    }
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

fn assert_canonical_settlement(dir: &Path, output: &Output, minimum_runs: usize) {
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let session_id = result["session_id"].as_str().expect("result session id");
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let mut started = std::collections::HashSet::new();
    let mut terminal = std::collections::HashSet::new();
    let mut request_completed = None;
    for line in persisted.lines().skip(1) {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(payload) = row["payload"].as_str() else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
        let body = &envelope["body"];
        match body["kind"].as_str() {
            Some("mag.run_started") => {
                started.insert(body["run_id"].as_str().unwrap().to_owned());
            }
            Some("mag.run_result") => {
                terminal.insert(body["run_id"].as_str().unwrap().to_owned());
            }
            Some("agentic_loop.request_completed")
                if body["request_id"] == result["request_id"] =>
            {
                request_completed = Some(body.clone())
            }
            _ => {}
        }
    }
    assert!(
        started.len() >= minimum_runs,
        "expected real child/descendant run"
    );
    assert_eq!(
        started, terminal,
        "every accepted MAG run must settle canonically"
    );
    let completion = request_completed.expect("correlated request completion must be durable");
    assert_eq!(
        completion["status"], result["status"],
        "stdout status must equal durable request completion"
    );
    assert_eq!(
        completion["error"], result["error"],
        "stdout error must equal durable request completion"
    );
    assert!(
        result["status"] == "interrupted" || result["status"] == "error",
        "CLI output is emitted only after sessions.flush_done"
    );
}

fn assert_authority_loss(dir: &Path, output: &Output) {
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "mag_authority_lost");
    assert_eq!(result["error"]["outcome"], "unknown");
    let session_id = result["session_id"].as_str().expect("result session id");
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let mut started = std::collections::HashSet::new();
    let mut terminal = std::collections::HashSet::new();
    let mut completion = None;
    for line in persisted.lines().skip(1) {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(payload) = row["payload"].as_str() else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
        let body = &envelope["body"];
        match body["kind"].as_str() {
            Some("mag.run_started") => {
                started.insert(body["run_id"].as_str().unwrap().to_owned());
            }
            Some("mag.run_result") => {
                terminal.insert(body["run_id"].as_str().unwrap().to_owned());
            }
            Some("agentic_loop.request_completed")
                if body["request_id"] == result["request_id"] =>
            {
                completion = Some(body.clone());
            }
            _ => {}
        }
    }
    assert!(
        !started.is_empty(),
        "MAG must die after accepting real work"
    );
    assert!(
        started.difference(&terminal).next().is_some(),
        "authority loss must not be fabricated as mag.run_result"
    );
    let completion = completion.expect("authority loss completion must be persisted before stdout");
    assert_eq!(completion["status"], result["status"]);
    assert_eq!(completion["error"], result["error"]);
}

fn success(out: &Output) -> String {
    assert!(
        out.status.success(),
        "status={:?}\nstderr={}\nstdout={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.stdout.contains(&0x1b), "no terminal escapes");
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn headless_starter_request_resume_tools_and_failures() {
    std::fs::create_dir_all(root().join("tmp")).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("headless-acceptance-")
        .tempdir_in(root().join("tmp"))
        .unwrap()
        .keep();
    for name in ["home", "config", "data", "cache", "bin"] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    for name in [
        "mag-plugin",
        "mock-plugin",
        "tool-gate",
        "basic-tools",
        "git-worktree",
    ] {
        let binary = binaries().join(name);
        assert!(
            binary.is_file(),
            "missing {}; run just build-headless",
            binary.display()
        );
        std::os::unix::fs::symlink(binary, dir.join("bin").join(name)).unwrap();
    }
    assert!(!dir.join("bin/nefor-tui").exists());
    std::fs::write(dir.join("README.md"), "headless fixture read result\n").unwrap();

    let first = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--prompt",
            "Summarise octopuses in one sentence.",
        ],
    );
    let text = success(&first);
    assert!(text.contains("Octopuses") && text.ends_with('\n'));
    let stderr = String::from_utf8(first.stderr).unwrap();
    let session_id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("session_id: "))
        .unwrap()
        .to_owned();
    assert_eq!(stderr.matches("session_id: ").count(), 1);
    let session_file = dir
        .join("data/nefor/sessions")
        .join(format!("{session_id}.jsonl"));
    assert!(
        session_file.is_file(),
        "initial CLI input must open persistence"
    );

    let resumed = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--resume",
            &session_id,
            "--prompt",
            "read readme",
            "--format",
            "json",
        ],
    );
    let result: serde_json::Value = serde_json::from_str(&success(&resumed)).unwrap();
    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["status"], "success");
    assert!(result["request_id"]
        .as_str()
        .unwrap()
        .starts_with("request-"));
    let persisted = std::fs::read_to_string(&session_file).unwrap();
    let mut submits = 0;
    let mut tools = 0;
    for line in persisted.lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(payload) = row["payload"].as_str() else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
        let body = &envelope["body"];
        if body["kind"] == "chat.input.submit" {
            submits += 1;
        }
        if body["kind"] == "conversation.fact.recorded" {
            if payload.contains("tool_") {
                tools += 1;
            }
            assert!(!payload.contains("\"display\":"));
        }
        assert_ne!(
            body["kind"], "tool.register",
            "no frontend display catalogs persisted"
        );
    }
    assert_eq!(submits, 2, "resume does not re-submit old input");
    assert!(tools > 0, "tool exchange remains canonical");

    let dispatch = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--mode",
            "yolo",
            "--format",
            "json",
            "--prompt",
            "summarise octopuses and lighthouses in parallel and combine into one paragraph",
        ],
    );
    let answer: serde_json::Value = serde_json::from_str(&success(&dispatch)).unwrap();
    assert!(answer["answer"]
        .as_str()
        .unwrap()
        .contains("steadfast lighthouse"));
    let dispatched_session = answer["session_id"].as_str().unwrap();
    assert!(dir
        .join("data/nefor/sessions")
        .join(dispatched_session)
        .join("mag")
        .is_dir());

    let error = run(
        &dir,
        &["--frontend", "cli", "--prompt", "fail", "--format", "json"],
    );
    assert_eq!(
        error.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&error.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&error.stdout).unwrap()["status"],
        "error"
    );
    let missing = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--resume",
            "does-not-exist",
            "--prompt",
            "hello",
        ],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    let usage = run(&dir, &["--frontend", "cli"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    let withheld = run_with_env(
        &dir,
        &[
            "--frontend",
            "cli",
            "--resume",
            &session_id,
            "--prompt",
            "This prompt must not submit.",
            "--format",
            "json",
        ],
        &[
            ("NEFOR_DEFAULT_PROVIDER", "unavailable-default"),
            ("NEFOR_TEST_PROVIDER_HELLO", "withhold"),
            ("NEFOR_STARTUP_TIMEOUT_MS", "8000"),
        ],
    );
    assert_eq!(withheld.status.code(), Some(1));
    let withheld_result: serde_json::Value = serde_json::from_slice(&withheld.stdout).unwrap();
    assert_eq!(withheld_result["status"], "error");
    assert!(withheld_result["error"]["message"]
        .as_str()
        .unwrap()
        .contains("mock-plugin"));
    assert_eq!(
        std::fs::read_to_string(&session_file)
            .unwrap()
            .matches("chat.input.submit")
            .count(),
        2,
        "historical readiness must not submit a new prompt"
    );

    let resumed_with_saved_provider = run_with_env(
        &dir,
        &[
            "--frontend",
            "cli",
            "--resume",
            &session_id,
            "--prompt",
            "Summarise octopuses in one sentence.",
            "--format",
            "json",
        ],
        &[
            ("NEFOR_DEFAULT_PROVIDER", "unavailable-default"),
            ("NEFOR_TEST_PROVIDER_HELLO", "delay"),
        ],
    );
    let saved_provider_result: serde_json::Value =
        serde_json::from_str(&success(&resumed_with_saved_provider)).unwrap();
    assert_eq!(saved_provider_result["status"], "success");
    assert_eq!(saved_provider_result["session_id"], session_id);

    let interrupted = run_inner(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--prompt",
            "summarise octopuses and lighthouses in parallel and combine into one paragraph INTERRUPTION_DESCENDANT",
        ],
        Intervention::Sigint { started_runs: 2 },
        &[],
    );
    assert_eq!(
        interrupted.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&interrupted.stdout).unwrap()["status"],
        "interrupted"
    );
    assert_canonical_settlement(&dir, &interrupted, 2);

    for (executable, marker, require_descendant) in [
        ("tool-gate", "TOOL_GATE_DEATH", false),
        ("mock-plugin", "SLOW_STREAM_REGRESSION_PROVIDER_DEATH", true),
    ] {
        let prompt = format!("SLOW_STREAM_REGRESSION_{marker}");
        let plugin_death = run_inner(
            &dir,
            &[
                "--frontend",
                "cli",
                "--format",
                "json",
                "--mode",
                "yolo",
                "--prompt",
                &prompt,
            ],
            Intervention::KillPlugin {
                executable,
                require_descendant,
            },
            &[],
        );
        assert_eq!(plugin_death.status.code(), Some(1));
        let plugin_result: serde_json::Value =
            serde_json::from_slice(&plugin_death.stdout).unwrap();
        assert_eq!(plugin_result["status"], "error");
        assert_eq!(plugin_result["error"]["code"], "plugin_terminated");
        assert!(plugin_result["error"]["message"]
            .as_str()
            .unwrap()
            .contains(executable));
        assert_canonical_settlement(&dir, &plugin_death, 1);
    }

    let mag_death = run_inner(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--prompt",
            "SLOW_STREAM_REGRESSION_MAG_AUTHORITY_DEATH",
        ],
        Intervention::KillPlugin {
            executable: "mag-plugin",
            require_descendant: false,
        },
        &[],
    );
    assert_eq!(mag_death.status.code(), Some(1));
    assert_authority_loss(&dir, &mag_death);
    println!("retained headless artifacts: {}", dir.display());
}

#[test]
fn shipped_mock_keeps_submission_receipts_within_activation() {
    use mlua::LuaSerdeExt;
    let lua = mlua::Lua::new();
    let nefor = lua.create_table().unwrap();
    nefor.set("name", "mock-plugin").unwrap();
    for name in ["on", "on_ready_ok"] {
        nefor
            .set(
                name,
                lua.create_function(|_, _: mlua::Variadic<mlua::Value>| Ok(()))
                    .unwrap(),
            )
            .unwrap();
    }
    let json = lua.create_table().unwrap();
    json.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })
        .unwrap(),
    )
    .unwrap();
    json.set(
        "encode",
        lua.create_function(|lua, value: mlua::Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })
        .unwrap(),
    )
    .unwrap();
    nefor.set("json", json).unwrap();
    lua.globals().set("nefor", nefor).unwrap();
    let source =
        std::fs::read_to_string(root().join("examples/nefor-agent/mock-provider/init.lua"))
            .unwrap();
    let select: mlua::Function = lua
        .load(format!("{source}\nreturn typed_response"))
        .eval()
        .unwrap();
    lua.globals().set("select_typed", select).unwrap();
    lua.load(r#"
        local instruction = {role="system",content='Expected canonical MAG JSON schema: {"type":"object","properties":{"content":{"type":"string"}}}. Write the value directly'}
        local valid = {role="tool",name="write_output",content={write="saved",validation={status="valid"}}}
        local invalid = {role="tool",name="write_output",content={write="saved",validation={status="invalid"}}}
        local request = {tools={"write_output","submit_output"}}
        assert(select_typed(request, {instruction,valid}).tool_calls[1].name == "submit_output")
        assert(select_typed(request, {instruction,valid,invalid}).tool_calls[1].name == "write_output")
        assert(select_typed(request, {instruction,valid,instruction}).tool_calls[1].name == "write_output")
    "#).exec().unwrap();
}
