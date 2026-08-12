use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("macOS test harness error: {error}");
            ExitCode::from(125)
        }
    }
}

fn run() -> io::Result<u8> {
    let mut args = env::args_os().skip(1);
    let first = args.next();
    let prepare_only = first.as_deref() == Some(std::ffi::OsStr::new("--prepare-only"));
    let timeout = if prepare_only {
        OsString::from("7200")
    } else {
        first.unwrap_or_else(|| OsString::from("7200"))
    };
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: nefor-cargo-test-harness [TIMEOUT_SECONDS | --prepare-only]",
        ));
    }
    let root = repository_root()?;
    let artifact_dir = unique_artifact_dir(&root.join("tmp/macos-test-signing"))?;
    let cargo_args = ["test", "--workspace", "--exclude", "nefor-tui"];

    let watchdog = cargo_target_dir(&root).join("debug/nefor-test-watchdog");
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
        .current_dir(&root)
        .args(["build", "--quiet", "-p", "nefor-test-watchdog"])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "could not build test watchdog: {status}"
        )));
    }

    #[cfg(target_os = "macos")]
    let prepared = {
        let helpers = nefor_cargo_test_harness::run_cargo_and_prepare(
            &root,
            &["build", "--workspace", "--exclude", "nefor-tui", "--bins"],
            None,
            &artifact_dir.join("runtime-helpers"),
        )?;
        let tests = nefor_cargo_test_harness::run_cargo_and_prepare(
            &root,
            &["test", "--workspace", "--exclude", "nefor-tui", "--no-run"],
            None,
            &artifact_dir.join("test-executables"),
        )?;
        let mut paths = helpers.paths;
        paths.extend(tests.paths);
        paths.sort();
        paths.dedup();
        nefor_cargo_test_harness::PreparedArtifacts { paths }
    };
    #[cfg(not(target_os = "macos"))]
    let prepared = nefor_cargo_test_harness::PreparedArtifacts { paths: Vec::new() };

    if prepare_only {
        eprintln!(
            "=== TEST PREPARE ONLY COMPLETE: {} executables ===",
            prepared.paths.len()
        );
        return Ok(0);
    }

    let status = Command::new(watchdog)
        .current_dir(&root)
        .arg("--phase")
        .arg("cargo-workspace-without-tui")
        .arg("--timeout-seconds")
        .arg(timeout)
        .arg("--")
        .arg(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
        .args(cargo_args)
        .status()?;
    nefor_cargo_test_harness::verify_paths(&prepared.paths).map_err(|error| {
        io::Error::other(format!(
            "a Cargo test artifact was replaced or lost its signature after prebuild: {error}"
        ))
    })?;
    status
        .code()
        .map_or(Ok(1), |code| Ok(u8::try_from(code).unwrap_or(1)))
}

fn repository_root() -> io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("harness manifest is not beneath repository root"))
}

fn unique_artifact_dir(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    for suffix in 0..100_u8 {
        let path = root.join(format!("{stamp}-{}-{suffix}", std::process::id()));
        if !path.exists() {
            fs::create_dir(&path)?;
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate signing artifact directory",
    ))
}

fn cargo_target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from)
}
