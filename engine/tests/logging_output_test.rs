use std::process::Command;

#[test]
fn file_logging_does_not_mirror_runtime_errors_to_stderr() {
    let root = tempfile::tempdir().expect("temp root");
    let config = root.path().join("config");
    let data = root.path().join("data");
    std::fs::create_dir_all(&config).expect("create config");
    std::fs::write(
        config.join("init.lua"),
        r#"
            function dispatch(_) end
            nefor.log.error("runtime-error-sentinel")
        "#,
    )
    .expect("write init.lua");

    let output = Command::new(env!("CARGO_BIN_EXE_nefor"))
        .args([
            "--config",
            config.to_str().expect("utf-8 config path"),
            "--data-dir",
            data.to_str().expect("utf-8 data path"),
            "plugin",
        ])
        .env_remove("NEFOR_LOG_STDERR")
        .output()
        .expect("run nefor");

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

    let log = std::fs::read_to_string(config.join("nefor.log")).expect("read nefor.log");
    assert!(
        log.contains("runtime-error-sentinel"),
        "runtime error missing from nefor.log: {log}"
    );
}
