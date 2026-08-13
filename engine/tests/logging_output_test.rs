use std::path::Path;
use std::process::{Command, Output};

fn write_config(root: &Path) -> std::path::PathBuf {
    let config = root.join("config");
    std::fs::create_dir_all(&config).expect("create config");
    std::fs::write(
        config.join("init.lua"),
        r#"
            function dispatch(_) end
            nefor.log.error("runtime-error-sentinel")
        "#,
    )
    .expect("write init.lua");
    config
}

fn run_nefor(config: &Path, data: &Path, extra_args: &[&str]) -> Output {
    let mut args = vec![
        "--config",
        config.to_str().expect("utf-8 config path"),
        "--data-dir",
        data.to_str().expect("utf-8 data path"),
    ];
    args.extend_from_slice(extra_args);
    args.push("plugin");

    Command::new(env!("CARGO_BIN_EXE_nefor"))
        .args(args)
        .env_remove("NEFOR_LOG_STDERR")
        .env_remove("NEFOR_LOG_FILE")
        .output()
        .expect("run nefor")
}

#[test]
fn default_file_logging_uses_data_root_and_not_config_dir() {
    let root = tempfile::tempdir().expect("temp root");
    let config = write_config(root.path());
    let data = root.path().join("data");

    let output = run_nefor(&config, &data, &[]);

    assert!(
        output.status.success(),
        "nefor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("runtime-error-sentinel"),
        "live runtime error leaked to stderr: {stderr}"
    );
    assert!(!config.join("nefor.log").exists());

    let log_path = data.join("logs/nefor.log");
    let log = std::fs::read_to_string(&log_path).expect("read data-root log");
    assert!(
        log.contains("runtime-error-sentinel"),
        "runtime error missing from {}: {log}",
        log_path.display()
    );
}

#[test]
fn explicit_log_file_is_used_exactly_and_creates_parents() {
    let root = tempfile::tempdir().expect("temp root");
    let config = write_config(root.path());
    let data = root.path().join("data");
    let selected = root.path().join("elsewhere/nested/custom.log");

    let output = run_nefor(
        &config,
        &data,
        &["--log-file", selected.to_str().expect("utf-8 log path")],
    );

    assert!(
        output.status.success(),
        "nefor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = std::fs::read_to_string(&selected).expect("read selected log");
    assert!(
        log.contains("runtime-error-sentinel"),
        "selected log: {log}"
    );
    assert!(!data.join("logs/nefor.log").exists());
    assert!(!config.join("nefor.log").exists());
}

#[test]
fn environment_log_file_is_used_exactly() {
    let root = tempfile::tempdir().expect("temp root");
    let config = write_config(root.path());
    let data = root.path().join("data");
    let selected = root.path().join("environment/custom.log");

    let output = Command::new(env!("CARGO_BIN_EXE_nefor"))
        .args([
            "--config",
            config.to_str().expect("utf-8 config path"),
            "--data-dir",
            data.to_str().expect("utf-8 data path"),
            "plugin",
        ])
        .env_remove("NEFOR_LOG_STDERR")
        .env("NEFOR_LOG_FILE", &selected)
        .output()
        .expect("run nefor");

    assert!(output.status.success());
    let log = std::fs::read_to_string(&selected).expect("read environment-selected log");
    assert!(
        log.contains("runtime-error-sentinel"),
        "selected log: {log}"
    );
    assert!(!data.join("logs/nefor.log").exists());
}

#[test]
fn stderr_mode_beats_explicit_log_file() {
    let root = tempfile::tempdir().expect("temp root");
    let config = write_config(root.path());
    let data = root.path().join("data");
    let selected = root.path().join("selected.log");

    let output = Command::new(env!("CARGO_BIN_EXE_nefor"))
        .args([
            "--config",
            config.to_str().expect("utf-8 config path"),
            "--data-dir",
            data.to_str().expect("utf-8 data path"),
            "--log-file",
            selected.to_str().expect("utf-8 log path"),
            "plugin",
        ])
        .env("NEFOR_LOG_STDERR", "1")
        .env_remove("NEFOR_LOG_FILE")
        .output()
        .expect("run nefor");

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("runtime-error-sentinel"),
        "runtime log missing from stderr: {stderr}"
    );
    assert!(!selected.exists());
    assert!(!data.join("logs/nefor.log").exists());
}

#[test]
fn selected_file_initialization_failure_names_path_and_does_not_fallback() {
    let root = tempfile::tempdir().expect("temp root");
    let config = write_config(root.path());
    let data = root.path().join("data");
    let blocked_parent = root.path().join("not-a-directory");
    std::fs::write(&blocked_parent, "file").expect("write blocker");
    let selected = blocked_parent.join("custom.log");

    let output = run_nefor(
        &config,
        &data,
        &["--log-file", selected.to_str().expect("utf-8 log path")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&selected.display().to_string()),
        "diagnostic did not name selected path: {stderr}"
    );
    assert!(!data.join("logs/nefor.log").exists());
    assert!(!config.join("nefor.log").exists());
}
