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
    // Test-only local classifier package keeps every process witness offline.
    // The distribution and its production archive pin remain unchanged.
    if !dir.join("fixture-config").exists() {
        assert!(Command::new("/bin/cp")
            .arg("-R")
            .arg(root().join("examples/nefor-agent"))
            .arg(dir.join("fixture-config"))
            .status()
            .unwrap()
            .success());
    }
    let da = dir.join("da-fixture/bin");
    std::fs::create_dir_all(&da).unwrap();
    let classifier = da.join("da");
    // Exit contract: 0 approve, 1 defer, 2 deny. The fixture defers a command
    // that creates a file and approves everything else. With
    // NEFOR_TEST_REAL_DA set to a released `da` binary the same tests run
    // against it instead, with the starter's own flags.
    let _ = std::fs::remove_file(&classifier);
    match std::env::var_os("NEFOR_TEST_REAL_DA") {
        Some(real) => std::os::unix::fs::symlink(real, &classifier).unwrap(),
        None => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &classifier,
                "#!/bin/sh\nif [ \"${1-}\" = \"--version\" ]; then echo \"da ${NEFOR_TEST_CLASSIFIER_VERSION:-0.3.0}\"; exit 0; fi\ncase \"$(cat)\" in *touch*) exit 1 ;; esac\nprintf 'allow\\n'\n",
            )
            .unwrap();
            std::fs::set_permissions(&classifier, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let init = dir.join("fixture-config/init.lua");
    let source = std::fs::read_to_string(&init).unwrap();
    let marker = "local da_package = require(\"libs.tool-validator.da\")";
    if !source.contains("-- local acceptance classifier") {
        assert_eq!(source.matches(marker).count(), 1);
        std::fs::write(
            init,
            source.replace(
                marker,
                &format!(
                    r#"{marker}
-- local acceptance classifier
local release_package = da_package.release_package
da_package.release_package = function(opts)
  local package = release_package(opts)
  package.archive = nil
  package.dir = {}
  return package
end
"#,
                    serde_json::to_string(&dir.join("da-fixture").display().to_string()).unwrap()
                ),
            ),
        )
        .unwrap();
    }
    let mut cmd = Command::new(binaries().join("nefor"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NEFOR_RUNTIME_ROOT", root())
        .env("NEFOR_EXECUTABLE_ROOT", dir.join("bin"))
        .env(
            "NEFOR_MOCK_PROVIDER_SCRIPT",
            if dir
                .join("fixture-config/mock-provider/acceptance.lua")
                .exists()
            {
                dir.join("fixture-config/mock-provider/acceptance.lua")
            } else {
                root().join("engine/tests/agentic_cli_mock_e2e/provider.lua")
            },
        )
        .env("NEFOR_STARTUP_TIMEOUT_MS", "10000")
        .env("OPENAI_PROVIDER_API_KEY", "offline-sentinel")
        .arg("--config")
        .arg(if dir.join("fixture-config").is_dir() {
            dir.join("fixture-config")
        } else {
            root().join("examples/nefor-agent")
        })
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
fn starter_rejects_wrong_classifier_version_before_starting_actors() {
    if std::env::var_os("NEFOR_TEST_REAL_DA").is_some() {
        return; // The injected wrong-version fixture is intentionally synthetic.
    }
    std::fs::create_dir_all(root().join("tmp")).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("wrong-version-")
        .tempdir_in(root().join("tmp"))
        .unwrap();
    for name in ["home", "config", "data", "cache", "bin"] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
    }
    let output = run_with_env(
        dir.path(),
        &["--frontend", "cli", "--prompt", "must not run"],
        &[("NEFOR_TEST_CLASSIFIER_VERSION", "0.2.0")],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("expected da 0.3.0") && error.contains("da 0.2.0"),
        "{error}"
    );
    assert!(
        !dir.path().join("data/nefor/sessions").exists(),
        "verification precedes sessions and actor startup"
    );
    assert!(output.stdout.is_empty());
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
    let recorded = std::fs::read_to_string(&session_file).unwrap();
    for shown in [
        "### Module `agents`",
        "type AgentConfig {",
        "### Module `nefor.contracts`",
        "### Module `nefor.shell`",
        "/lib/nefor/workflow.mag` (read when needed)",
        "/lib/nefor.mag` (read when needed)",
        "`nefor.general_tools` — policy `ToolApprovalPolicy.Default`:",
        "`nefor.read_only_tools` — policy `ToolApprovalPolicy.ReadOnly`:",
        "- `mag-write-file`",
    ] {
        assert!(
            recorded.contains(shown),
            "the lead prompt shows the starter's public MAG surface: {shown}"
        );
    }
    assert!(
        !recorded.contains("- `agents.models`") && !recorded.contains("- `nefor.graph`"),
        "internal MAG modules are not advertised to the lead"
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
    assert!(
        answer["answer"]
            .as_str()
            .unwrap()
            .contains("steadfast lighthouse"),
        "{answer}"
    );
    let dispatched_session = answer["session_id"].as_str().unwrap();
    let session_mag = dir
        .join("data/nefor/sessions")
        .join(dispatched_session)
        .join("mag");
    assert!(
        session_mag.join("lead/octo-lighthouse.mag").is_file(),
        "the lead authors into its own workspace"
    );
    assert!(
        !session_mag.join("octo-lighthouse.mag").exists() && !session_mag.join("mag.toml").exists(),
        "nothing is authored into the shared session folder"
    );
    // The provider script answers only after the combining node wrote and
    // compiled its own source and was refused the lead's file by name. Its
    // workspace sits in its run's folder, beside the runtime's output files.
    let authoring_runs: Vec<_> = std::fs::read_dir(session_mag.join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|run| run.join("combine").is_dir())
        .collect();
    assert_eq!(authoring_runs.len(), 1, "one run had an authoring node");
    let run_dir = &authoring_runs[0];
    assert!(
        run_dir.join("combine/combine-notes.mag").is_file()
            && run_dir.join("combine/mag.toml").is_file(),
        "a workspace is its run and node path, and is its own project"
    );
    assert!(
        run_dir.join("combine.llm.output").is_file() && !session_mag.join("agents").exists(),
        "the run's folder holds both its outputs and its nodes' source"
    );

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
        .load(format!("package.preload[\"libs.mock-provider\"] = function() return {{ serve = function(opts) captured_respond = opts.respond end, tool_call = function(name,args) return {{ tool_calls = {{{{name=name,arguments=args}}}} }} end, text=function(text) return {{text=text}} end, error=function(text) return {{error=text}} end }} end\n{source}\nreturn captured_respond"))
        .eval()
        .unwrap();
    lua.globals().set("select_typed", select).unwrap();
    lua.load(r#"
        local instruction = {role="system",content="Summarise octopuses in one sentence."}
        local valid = {role="tool",name="write_output",content={write="saved",validation={status="valid"}}}
        local invalid = {role="tool",name="write_output",content={write="saved",validation={status="invalid"}}}
        local function select(messages) return select_typed({system=instruction.content,messages=messages}) end
        assert(select({instruction,valid}).tool_calls[1].name == "submit_output")
        assert(select({instruction,valid,invalid}).tool_calls[1].name == "write_output")
        assert(select({instruction,valid,instruction}).tool_calls[1].name == "write_output")
    "#).exec().unwrap();
}

#[test]
fn rejected_provider_frame_closes_whole_request_and_resume_remains_usable() {
    let dir = tempfile::Builder::new()
        .prefix("owner-admission-")
        .tempdir_in(root().join("tmp"))
        .unwrap()
        .keep();
    for name in [
        "home",
        "config",
        "data",
        "cache",
        "bin",
        "fixture-config/mock-provider",
    ] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    for name in [
        "mag-plugin",
        "mock-plugin",
        "tool-gate",
        "basic-tools",
        "git-worktree",
    ] {
        std::os::unix::fs::symlink(binaries().join(name), dir.join("bin").join(name)).unwrap();
    }
    for entry in std::fs::read_dir(root().join("examples/nefor-agent")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "mock-provider" {
            assert!(Command::new("/bin/cp")
                .arg("-R")
                .arg(entry.path())
                .arg(dir.join("fixture-config").join(entry.file_name()))
                .status()
                .unwrap()
                .success());
        }
    }
    std::fs::write(
        dir.join("fixture-config/mock-provider/acceptance.lua"),
        r#"
local mock = require("libs.mock-provider")
mock.serve { respond = function(request)
  if os.getenv("OWNER_REJECT_FRAME") == "1" then
    local ok = pcall(nefor.emit, "completion.event", {
      request_id = request.id, event = "text_delta", text = string.rep("x", 17 * 1024 * 1024),
    })
    assert(not ok, "fixture must exercise actual output admission")
    return mock.error("output admission rejected; prior effects unknown", { effects = "unknown" })
  end
  return mock.text("Octopuses are intelligent cephalopods.")
end }
"#,
    )
    .unwrap();
    let rejected = run_with_env(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--prompt",
            "Owner frame rejection",
        ],
        &[("OWNER_REJECT_FRAME", "1")],
    );
    assert_eq!(
        rejected.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_canonical_settlement(&dir, &rejected, 1);
    let session = result["session_id"].as_str().unwrap();
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    let bodies = persisted
        .lines()
        .filter_map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).ok()?;
            let envelope: serde_json::Value =
                serde_json::from_str(row["payload"].as_str()?).ok()?;
            Some(envelope["body"].clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body["kind"] == "agentic_loop.request_completed"
                && body["request_id"] == result["request_id"])
            .count(),
        1
    );
    assert!(bodies
        .iter()
        .any(|body| body["kind"] == "conversation.fact.recorded"
            && body["event"]["kind"] == "turn_failed"
            && body["event"]["detail"]["effects"] == "unknown"));
    assert!(
        !persisted.contains(&"x".repeat(1024)),
        "rejected progress never entered the bus"
    );
    let resumed = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--resume",
            session,
            "--prompt",
            "Summarise octopuses in one sentence.",
        ],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success(&resumed)).unwrap()["status"],
        "success"
    );
}

// Only fixture configuration is altered. Routing, providers, tool-gate, MAG,
// conversation authority, sessions and CLI are the shipped processes/owners.
fn isolated_starter(prefix: &str) -> PathBuf {
    std::fs::create_dir_all(root().join("tmp")).unwrap();
    let dir = tempfile::Builder::new()
        .prefix(prefix)
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
        std::os::unix::fs::symlink(binaries().join(name), dir.join("bin").join(name)).unwrap();
    }
    assert!(Command::new("/bin/cp")
        .arg("-R")
        .arg(root().join("examples/nefor-agent"))
        .arg(dir.join("fixture-config"))
        .status()
        .unwrap()
        .success());
    let init = dir.join("fixture-config/init.lua");
    let original = std::fs::read_to_string(&init).unwrap();
    std::fs::write(
        init,
        format!(
            "{original}\n{}",
            r#"
nefor.bus.on_event("*", function(entry)
  local ok, env = pcall(nefor.json.decode, entry.payload or "")
  local body = ok and type(env) == "table" and env.body
  if type(body) ~= "table" then return end
  if body.kind == "sessions.transition_failed" or body.kind == "sessions.resume_done"
      or body.kind == "sessions.flush_request" or body.kind == "sessions.flush_done"
      or body.kind == "tool.result" or body.kind == "basic-tools.tool.invoke"
      or body.kind == "tool-gate.tool.invoke" then
    local f = assert(io.open("boundary-trace.jsonl", "a"))
    f:write(nefor.json.encode({from=env.from,body=body}), "\n"); f:close()
  end
end)
"#
        ),
    )
    .unwrap();
    dir
}

#[test]
fn noisy_real_tool_result_is_bounded_omitted_and_accepts_later_work() {
    let dir = isolated_starter("tool-output-admission-");
    let turn = dir.join("fixture-config/agentic-loop/lead-turn.mag");
    let turn_source = std::fs::read_to_string(&turn).unwrap();
    std::fs::write(
        turn,
        turn_source.replace("tools: [", "tools: [\"shell.script\", "),
    )
    .unwrap();
    std::fs::write(dir.join("README.md"), "later operation").unwrap();
    std::fs::write(dir.join("fixture-config/mock-provider/acceptance.lua"), r#"
local mock = require("libs.mock-provider")
mock.serve { respond = function(request)
  local last = request.messages[#request.messages]
  if last.role == "tool" and last.name == "shell.script" then
    return mock.tool_call("read_file", {path="README.md"})
  end
  if last.role == "tool" and last.name == "read_file" then
    return mock.text("bounded result and later operation completed")
  end
  if tostring(last.content) == "read readme" then return mock.tool_call("read_file", {path="README.md"}) end
  return mock.tool_call("shell.script", {script="head -c 17825792 /dev/zero | tr '\\000' x", cwd=".",
    timeout={present=true,milliseconds=10000}})
end }
"#).unwrap();
    let output = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--mode",
            "yolo",
            "--format",
            "json",
            "--prompt",
            "GIANT_TOOL_OUTPUT",
        ],
    );
    let result: serde_json::Value = serde_json::from_str(&success(&output)).unwrap();
    let session = result["session_id"].as_str().unwrap();
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    assert!(persisted.contains("bytes of output omitted"));
    assert!(!persisted.contains("frame_too_large"));
    let trace = std::fs::read_to_string(dir.join("boundary-trace.jsonl")).unwrap();
    let trace = trace
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let invoke = trace
        .iter()
        .find(|row| row["body"]["kind"] == "basic-tools.tool.invoke")
        .unwrap();
    let inner = &invoke["body"]["id"];
    let native = trace
        .iter()
        .filter(|row| {
            row["from"] == "basic-tools"
                && row["body"]["kind"] == "tool.result"
                && row["body"]["id"] == *inner
        })
        .collect::<Vec<_>>();
    assert_eq!(native.len(), 1);
    let process = &native[0]["body"]["output"];
    let bounded = serde_json::to_string(process).unwrap();
    let text = process["stdout"].as_str().unwrap();
    assert!(text.contains("16777216 bytes of output omitted"));
    assert!(text.starts_with(&"x".repeat(512 * 1024)));
    assert!(text.ends_with(&"x".repeat(512 * 1024)));
    assert!(bounded.len() < 2 * 1024 * 1024);
    assert!(native[0]["body"]["error"].is_null());
    assert!(
        trace
            .iter()
            .any(|row| row["body"]["kind"] == "basic-tools.tool.invoke"
                && row["body"]["name"] == "read_file"),
        "later work in same physical tool process"
    );
    assert!(trace.iter().any(|row| row["from"] == "tool-gate"
        && row["body"]["kind"] == "tool.result"
        && row["body"]["error"] == native[0]["body"]["error"]));
    assert!(trace
        .iter()
        .any(|row| row["body"]["kind"] == "sessions.flush_done"
            && row["body"]["request_id"] == result["request_id"]));
    assert_eq!(
        persisted.matches("agentic_loop.request_completed").count(),
        1
    );
    // A subsequent real basic-tools operation, not only another provider turn.
    std::fs::write(dir.join("README.md"), "later operation").unwrap();
    let resumed = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--resume",
            session,
            "--prompt",
            "read readme",
        ],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success(&resumed)).unwrap()["status"],
        "success"
    );
    println!("retained oversized tool artifacts: {}", dir.display());
}

/// A model that calls a tool it cannot use is told which tools it can call.
/// The lead and a delegated typed worker each try a name that exists nowhere
/// for them and a real tool outside their allowlist; both get one message
/// listing exactly what their own request offered, in that order.
#[test]
fn unusable_tool_call_answers_with_the_callers_available_tools() {
    let dir = isolated_starter("unknown-tool-");
    std::fs::copy(
        root().join("engine/tests/agentic_cli_mock_e2e/unknown_tool_provider.lua"),
        dir.join("fixture-config/mock-provider/acceptance.lua"),
    )
    .unwrap();
    let output = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--mode",
            "yolo",
            "--format",
            "json",
            "--prompt",
            "UNKNOWN_TOOL_PROBE",
        ],
    );
    let result: serde_json::Value = serde_json::from_str(&success(&output)).unwrap();
    assert!(
        !dir.join("must-not-exist.txt").exists(),
        "a tool outside the allowlist never runs"
    );

    // The lead's list is its toolset's tools, in authored order, followed by
    // the runtime's own `self_compact`, which every model request is offered.
    let lead_tools = "read_file, read_image, mag-status, mag-await, mag-terminate, \
                      write-review, mag-write-file, mag-preview, mag-apply, self_compact";
    assert_eq!(
        result["answer"].as_str().unwrap().trim_end(),
        format!(
            "LEAD <[tool error] unknown tool `bash`; available tools: {lead_tools}> \
             <[tool error] unknown tool `write_file`; available tools: {lead_tools}>"
        )
    );

    // The worker's allowlist is `read_image, bash, read_file`: the unadvertised
    // `bash` drops out and the typed-result tools its request added follow.
    let worker_tools = "read_image, read_file, write_output, submit_output, self_compact";
    let session = result["session_id"].as_str().unwrap();
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    let report = format!(
        "WORKER <[tool error] unknown tool `bash`; available tools: {worker_tools}> \
         <[tool error] unknown tool `write_file`; available tools: {worker_tools}>"
    );
    assert!(
        persisted.contains(&report),
        "worker report missing; retained artifacts: {}",
        dir.display()
    );
    let trace = std::fs::read_to_string(dir.join("boundary-trace.jsonl")).unwrap();
    assert!(
        !trace.contains("basic-tools.tool.invoke"),
        "no rejected call reaches a tool source"
    );
}

/// The file name a model gives `mag-apply` is the name every stage sees: the
/// recorded tool call, the gate's invocation, the compiler's entry and the
/// file in the lead's workspace. The source is the one a model sent in a
/// recorded session, which itself mentions a `.mag` suffix. A file with no
/// name before its extension is then refused by each MAG source tool in one
/// sentence, and nothing is written or compiled for it.
#[test]
fn mag_apply_keeps_the_file_name_the_model_gave() {
    let dir = isolated_starter("mag-file-name-");
    let content =
        include_str!("../../plugins/openai-provider/tests/fixtures/mag_apply_locate_content.mag");
    let provider = std::fs::read_to_string(
        root().join("engine/tests/agentic_cli_mock_e2e/mag_file_name_provider.lua"),
    )
    .unwrap();
    assert_eq!(provider.matches("__MAG_PROGRAM__").count(), 1);
    std::fs::write(
        dir.join("fixture-config/mock-provider/acceptance.lua"),
        provider.replace("__MAG_PROGRAM__", content),
    )
    .unwrap();
    let output = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--mode",
            "yolo",
            "--format",
            "json",
            "--prompt",
            "MAG_FILE_NAME_PROBE",
        ],
    );
    let result: serde_json::Value = serde_json::from_str(&success(&output)).unwrap();
    let answer = result["answer"].as_str().unwrap();
    let (applied, refused) = answer.split_once("\n<").unwrap();
    assert!(
        applied.starts_with("APPLIED ") && !applied.contains("[tool error]"),
        "the workflow ran: {applied}"
    );
    let unnamed = |tool: &str, file: &str| {
        format!(
            "[tool error] {tool}: argument `file` must name a file, like `inspect.mag`; \
             got `{file}`, which has no name before the extension"
        )
    };
    assert_eq!(
        format!("<{}", refused.trim_end()),
        format!(
            "<{}>\n<{}>\n<{}>",
            unnamed("mag-write-file", ".mag"),
            unnamed("mag-preview", "dir/.mag"),
            unnamed("mag-apply", ".magl")
        )
    );

    let session = result["session_id"].as_str().unwrap();
    let sessions = dir.join("data/nefor/sessions");
    let workspace = sessions.join(session).join("mag/lead");
    assert_eq!(
        std::fs::read_to_string(workspace.join("locate.mag")).unwrap(),
        content
    );
    for unnamed in [".mag", "dir", ".magl"] {
        assert!(
            !workspace.join(unnamed).exists(),
            "nothing is written for a file without a name: {unnamed}"
        );
    }

    // Every recorded stage of the call names the same file.
    let persisted = std::fs::read_to_string(sessions.join(format!("{session}.jsonl"))).unwrap();
    let mut stages = Vec::new();
    for line in persisted.lines() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        let Some(payload) = entry["payload"].as_str() else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
        let body = &envelope["body"];
        let kind = body["kind"].as_str().unwrap_or_default();
        let file = match kind {
            "conversation.fact.recorded" => match body["event"]["kind"].as_str() {
                Some("tool_call_fragment_appended") => {
                    &body["event"]["fragment"]["arguments"]["file"]
                }
                Some("tool_call_completed") => &body["event"]["call"]["arguments"]["file"],
                _ => continue,
            },
            "tool-gate.tool.invoke"
            | "lead-workflow.tool.prepare"
            | "lead-workflow.tool.invoke"
                if body["name"] == "mag-apply" =>
            {
                &body["args"]["file"]
            }
            // The starter compiles through the project build; a configuration
            // without one sends `mag.load` with the same entry.
            "mag.build" | "mag.load" if envelope["from"] == "lead-workflow" => &body["entry"],
            _ => continue,
        };
        let stage = match kind {
            "conversation.fact.recorded" => body["event"]["kind"].as_str().unwrap(),
            "mag.build" | "mag.load" => "compile",
            kind => kind,
        };
        // The refused calls are recorded as the model made them and reach
        // the tool's argument check, and no further.
        if file != "locate.mag" {
            assert!(
                matches!(
                    stage,
                    "tool_call_fragment_appended"
                        | "tool_call_completed"
                        | "tool-gate.tool.invoke"
                        | "lead-workflow.tool.prepare"
                ),
                "{stage} never sees a file without a name: {body}"
            );
            continue;
        }
        stages.push(stage.to_owned());
    }
    for stage in [
        "tool_call_fragment_appended",
        "tool_call_completed",
        "tool-gate.tool.invoke",
        "lead-workflow.tool.prepare",
        "compile",
        "lead-workflow.tool.invoke",
    ] {
        assert!(
            stages.iter().any(|seen| seen == stage),
            "{stage} was recorded; saw {stages:?}"
        );
    }
}

#[test]
fn routed_replay_reader_fault_closes_cli_and_reader_without_submission() {
    let dir = isolated_starter("routed-replay-fault-");
    let first = run(
        &dir,
        &["--frontend", "cli", "--format", "json", "--prompt", "hello"],
    );
    let first: serde_json::Value = serde_json::from_str(&success(&first)).unwrap();
    let session = first["session_id"].as_str().unwrap();
    let init = dir.join("fixture-config/init.lua");
    let original = std::fs::read_to_string(&init).unwrap();
    // Fail the physical file-reader boundary deterministically after size sampling.
    // The real sessions handler and NCP/CLI routing remain untouched.
    let shim = r#"
if os.getenv("REPLAY_READER_FAULT") == "1" then
  local open = io.open
  io.open = function(path, mode)
    local fh, err = open(path, mode)
    if not fh or mode ~= "r" or not tostring(path):match("%.jsonl$") then return fh,err end
    local sampled = false
    return {
      seek = function(_, whence, offset)
        if whence == "end" then sampled = true end
        return fh:seek(whence, offset)
      end,
      read = function(_, ...)
        if sampled then return nil, "deterministic replay EIO" end
        return fh:read(...)
      end,
      close = function(_)
        if sampled then
          local proof = assert(open("reader-closed", "a")); proof:write("closed\n"); proof:close()
        end
        return fh:close()
      end,
    }
  end
end
"#;
    std::fs::write(init, format!("{shim}\n{original}")).unwrap();
    std::fs::write(dir.join("boundary-trace.jsonl"), "").unwrap();
    let fault = run_with_env(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--resume",
            session,
            "--prompt",
            "MUST_NOT_SUBMIT_AFTER_REPLAY_FAULT",
        ],
        &[("REPLAY_READER_FAULT", "1")],
    );
    assert_eq!(fault.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&fault.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "session_failure");
    assert!(result["error"]["message"]
        .as_str()
        .unwrap()
        .contains("deterministic replay EIO"));
    assert!(result["request_id"]
        .as_str()
        .unwrap()
        .starts_with("request-"));
    assert_eq!(
        std::fs::read_to_string(dir.join("reader-closed")).unwrap(),
        "closed\n"
    );
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    assert!(!persisted.contains("MUST_NOT_SUBMIT_AFTER_REPLAY_FAULT"));
    let trace = std::fs::read_to_string(dir.join("boundary-trace.jsonl")).unwrap();
    let rows = trace
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let failures = rows
        .iter()
        .filter(|row| row["body"]["kind"] == "sessions.transition_failed")
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["body"]["session_id"], session);
    assert!(!rows
        .iter()
        .any(|row| row["body"]["kind"] == "sessions.resume_done"));
    assert_eq!(failures[0]["body"]["request_id"], result["request_id"]);
    assert_eq!(
        rows.iter()
            .filter(|row| row["body"]["kind"] == "sessions.flush_done"
                && row["body"]["request_id"] == result["request_id"])
            .count(),
        1
    );
    let evidence = serde_json::json!({
        "cli_request_id": result["request_id"], "cli_status": result["status"],
        "transition_request_id": failures[0]["body"]["request_id"],
        "correlated_flush_done": rows.iter().any(|row| row["body"]["kind"] == "sessions.flush_done" && row["body"]["request_id"] == result["request_id"]),
    });
    std::fs::write(
        dir.join("replay-loss-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    let recovered = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--resume",
            session,
            "--prompt",
            "hello",
        ],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success(&recovered)).unwrap()["status"],
        "success"
    );
    println!("retained routed replay fault artifacts: {}", dir.display());
}

#[test]
fn accepted_real_mag_giant_join_has_small_authoritative_terminal() {
    let dir = isolated_starter("mag-output-admission-");
    let branches = (0..18).map(|index| format!(
        "script(\"branch-{index}\", ShellScriptParams {{script: \"head -c 9437184 /dev/zero | tr '\\\\000' x\", cwd: \".\", timeout: Timeout.Seconds(10)}})"
    )).collect::<Vec<_>>().join(",\n");
    let program = format!("import nefor.{{compile_graph, script, ShellScriptParams, Timeout, sequence}}\ncompile_graph(sequence([{branches}]))\n");
    let behavior = format!(
        r#"
local mock = require("libs.mock-provider")
local program = [=[{program}]=]
mock.serve {{ respond = function(request)
  local last = request.messages[#request.messages]
  if last.role == "tool" and last.name == "mag-write-file" then
    return mock.tool_call("mag-apply", {{file="giant-join.mag"}})
  end
  if last.role == "tool" or nefor.json.encode(last.content):find("mag_run", 1, true) or nefor.json.encode(last.content):find("Summarise octopuses in one sentence.", 1, true) then
    return mock.text("observed bounded MAG terminal")
  end
  return mock.tool_call("mag-write-file", {{file="giant-join.mag",new_string=program}})
end }}
"#
    );
    std::fs::write(
        dir.join("fixture-config/mock-provider/acceptance.lua"),
        behavior,
    )
    .unwrap();
    let output = run(
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
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let session = result["session_id"].as_str().unwrap();
    let persisted = std::fs::read_to_string(
        dir.join("data/nefor/sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    let bodies = persisted
        .lines()
        .filter_map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).ok()?;
            let env: serde_json::Value = serde_json::from_str(row["payload"].as_str()?).ok()?;
            Some(env["body"].clone())
        })
        .collect::<Vec<_>>();
    let terminals = bodies
        .iter()
        .filter(|body| body["kind"] == "mag.run_result" && body["failure"] == "output_delivery")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "accepted giant MAG output must yield one small authoritative terminal; artifacts {}",
        dir.display()
    );
    let run_id = &terminals[0]["run_id"];
    assert!(bodies
        .iter()
        .any(|body| body["kind"] == "mag.run_started" && body["run_id"] == *run_id));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["run_id"], *run_id);
    assert_eq!(result["error"]["code"], "mag_run_failed");
    assert_eq!(terminals[0]["status"], "failed");
    assert!(terminals[0]["result"].is_null());
    assert_eq!(terminals[0]["persisted"], true);
    let output_path = terminals[0]["output_path"].as_str().unwrap();
    assert!(std::fs::metadata(output_path).unwrap().len() >= 9 * 1024 * 1024);
    let failed_index = bodies
        .iter()
        .position(|body| body["kind"] == "mag.run_result" && body["run_id"] == *run_id)
        .unwrap();
    assert!(
        bodies[failed_index + 1..]
            .iter()
            .any(|body| body["kind"] == "mag.run_started"),
        "same MAG process accepts the subsequent deferred lead turn"
    );
    assert!(
        bodies
            .iter()
            .any(|body| body["kind"] == "mag.actor_killed" && body["run_id"] == *run_id),
        "failed run reclaims its actors"
    );
    assert!(terminals[0]["error"]
        .as_str()
        .unwrap()
        .contains("effects may have occurred"));
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body["kind"] == "agentic_loop.request_completed"
                && body["request_id"] == result["request_id"])
            .count(),
        1
    );
    let resumed = run(
        &dir,
        &[
            "--frontend",
            "cli",
            "--format",
            "json",
            "--resume",
            session,
            "--prompt",
            "Summarise octopuses in one sentence.",
        ],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success(&resumed)).unwrap()["status"],
        "success"
    );
    println!("retained accepted MAG output artifacts: {}", dir.display());
}

/// An agent's approval policy is enforced end to end. The lead (Default)
/// applies a workflow holding a ReadOnly worker. The worker applies a workflow
/// whose command only reads: it is approved as a unit and its command runs
/// without anyone being asked. The worker then applies a workflow whose
/// command writes: the gate refuses it with an explanation and the command
/// never runs. No permission mode widens the worker.
#[test]
fn a_read_only_agents_workflows_are_decided_by_its_policy_in_every_mode() {
    for mode in ["safe", "auto", "yolo"] {
        let dir = isolated_starter("approval-policy-");
        std::fs::copy(
            root().join("engine/tests/agentic_cli_mock_e2e/approval_policy_provider.lua"),
            dir.join("fixture-config/mock-provider/acceptance.lua"),
        )
        .unwrap();
        let output = run(
            &dir,
            &[
                "--frontend",
                "cli",
                "--mode",
                mode,
                "--format",
                "json",
                "--prompt",
                "APPROVAL_POLICY_PROBE",
            ],
        );
        let result: serde_json::Value = serde_json::from_str(&success(&output)).unwrap();
        assert_eq!(result["status"], "success", "{mode}: {result}");
        assert!(
            !dir.join("approval-marker.txt").exists(),
            "{mode}: the writing command never ran"
        );

        let session = result["session_id"].as_str().unwrap();
        let persisted = std::fs::read_to_string(
            dir.join("data/nefor/sessions")
                .join(format!("{session}.jsonl")),
        )
        .unwrap();
        let events = persisted
            .lines()
            .filter_map(|line| {
                let row: serde_json::Value = serde_json::from_str(line).ok()?;
                serde_json::from_str::<serde_json::Value>(row["payload"].as_str()?).ok()
            })
            .collect::<Vec<_>>();
        let bodies = events
            .iter()
            .map(|event| &event["body"])
            .collect::<Vec<_>>();
        let applies = bodies
            .iter()
            .filter(|body| body["kind"] == "tool-gate.tool.invoke" && body["name"] == "mag-apply")
            .collect::<Vec<_>>();
        assert_eq!(
            applies.len(),
            3,
            "{mode}: the lead applies once, the worker twice"
        );

        // The lead is a Default agent and is unaffected: its workflow, which
        // only holds a narrower agent, runs.
        assert_eq!(applies[0]["approval_policy"], "Default", "{mode}");
        assert_eq!(applies[0]["args"]["file"], "outer.mag", "{mode}");
        // The worker's calls carry the worker's own policy.
        assert_eq!(applies[1]["approval_policy"], "ReadOnly", "{mode}");
        assert_eq!(applies[2]["approval_policy"], "ReadOnly", "{mode}");

        let result_of = |call: &serde_json::Value| {
            bodies
                .iter()
                .find(|body| body["kind"] == "tool.result" && body["id"] == call["id"])
                .unwrap_or_else(|| panic!("{mode}: no result for {}", call["id"]))
        };
        assert!(
            result_of(applies[0])["error"].is_null(),
            "{mode}: {}",
            result_of(applies[0])
        );
        assert!(
            result_of(applies[1])["error"].is_null(),
            "{mode}: the read-only workflow is approved: {}",
            result_of(applies[1])
        );
        assert_eq!(
            result_of(applies[2])["error"],
            concat!(
                "tool `mag-apply` was not run: your approval policy is ReadOnly and the workflow ",
                "declares 1 node not shown to be allowed under the ReadOnly approval policy:\n",
                "- node `leave-a-mark` (shell script) `touch approval-marker.txt`: ",
                "not shown to be read-only"
            ),
            "{mode}: the model is told why its workflow was not run"
        );

        // The approved workflow's command ran as a node of that workflow: it
        // reached the tool source marked as such, with no policy of its own,
        // and nobody was asked about it.
        let commands = bodies
            .iter()
            .filter(|body| body["kind"] == "basic-tools.tool.invoke")
            .collect::<Vec<_>>();
        assert_eq!(
            commands.len(),
            1,
            "{mode}: exactly one command ran: {commands:?}"
        );
        assert_eq!(commands[0]["name"], "shell.script", "{mode}");
        assert_eq!(
            commands[0]["args"]["script"], "printf 'read-only-ran'",
            "{mode}"
        );
        assert_eq!(commands[0]["workflow_node"], true, "{mode}");
        assert!(commands[0]["approval_policy"].is_null(), "{mode}");
        assert!(
            bodies
                .iter()
                .any(|body| body["kind"] == "tool.result"
                    && body["output"]["stdout"] == "read-only-ran"),
            "{mode}: the read-only command produced its output"
        );
        assert!(
            !bodies
                .iter()
                .any(|body| body["kind"] == "basic-tools.tool.prepare"),
            "{mode}: a node of an approved workflow is not decided again"
        );
        assert!(
            !bodies.iter().any(|body| {
                body["kind"] == "chat.tool.permission_request"
                    || body["kind"] == "chat.tool.popup_request"
            }),
            "{mode}: a ReadOnly agent is never asked about"
        );
        // Each agent's request describes mag-apply for that agent's policy:
        // the ReadOnly worker is told what is rejected, the Default lead gets
        // the registered description with nothing appended.
        let descriptions = std::fs::read_to_string(dir.join("approval-descriptions.txt")).unwrap();
        let described = |who: &str| {
            descriptions
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{who}=")))
                .unwrap_or_else(|| panic!("{mode}: no description noted for {who}: {descriptions}"))
                .to_owned()
        };
        assert_eq!(described("lead"), "REGISTERED", "{mode}");
        let worker_description = described("worker");
        assert!(
            worker_description.starts_with("Runs MAG code.")
                && worker_description.ends_with(
                    "Your approval policy is ReadOnly: a workflow that declares nodes that are \
                     not read-only, or agents with a wider approval policy than yours, is rejected."
                ),
            "{mode}: {worker_description}"
        );

        // The writing workflow's file was written by prepare into the worker's
        // own node workspace, inside the run that holds the worker, and it
        // stays there for the author to fix. The lead's workspace holds only
        // the lead's file.
        let mag_dir = dir.join("data/nefor/sessions").join(session).join("mag");
        let outer_run = bodies
            .iter()
            .find(|body| body["kind"] == "mag.execute" && body["invocation_label"] == "outer.mag")
            .and_then(|body| body["run_id"].as_str())
            .unwrap_or_else(|| panic!("{mode}: the outer run was executed"));
        let worker_dir = mag_dir.join("runs").join(outer_run).join("worker");
        assert!(
            worker_dir.join("writes.mag").is_file(),
            "{mode}: the refused workflow's source stays in the worker's workspace"
        );
        assert!(worker_dir.join("reads.mag").is_file(), "{mode}");
        assert!(mag_dir.join("lead/outer.mag").is_file(), "{mode}");
        assert!(
            !mag_dir.join("lead/writes.mag").exists() && !mag_dir.join("writes.mag").exists(),
            "{mode}: a node's source is not written into the lead's or the session's folder"
        );
    }
}
