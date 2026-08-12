use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_TIMEOUT_SECONDS: u64 = 7_200;
const TIMEOUT_EXIT_CODE: u8 = 124;
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(2);

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

#[derive(Debug)]
struct Config {
    phase: String,
    timeout: Duration,
    artifact_root: PathBuf,
    command: Vec<OsString>,
}

#[derive(Debug)]
struct RunResult {
    status: ExitStatus,
    timed_out: bool,
    #[cfg(test)]
    artifact_dir: PathBuf,
}

fn main() -> ExitCode {
    match parse_args(env::args_os().skip(1)).and_then(|config| run(&config)) {
        Ok(result) => {
            if result.timed_out {
                ExitCode::from(TIMEOUT_EXIT_CODE)
            } else if let Some(code) = result.status.code() {
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            } else {
                let code = result.status.signal().map_or(1, |signal| 128 + signal);
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            }
        }
        Err(error) => {
            eprintln!("watchdog error: {error}");
            ExitCode::from(125)
        }
    }
}

fn parse_args(args: impl Iterator<Item = OsString>) -> io::Result<Config> {
    let mut args = args.peekable();
    let mut phase = None;
    let mut timeout = env::var("NEFOR_TEST_TIMEOUT_SECONDS")
        .ok()
        .map(|raw| parse_timeout(&raw))
        .transpose()?
        .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS));
    let mut artifact_root = repository_root()?.join("tmp/test-watchdog");

    loop {
        let Some(arg) = args.next() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, usage()));
        };
        if arg == "--" {
            break;
        }
        match arg.to_str() {
            Some("--phase") => phase = Some(required_utf8(&mut args, "--phase")?),
            Some("--timeout-seconds") => {
                timeout = parse_timeout(&required_utf8(&mut args, "--timeout-seconds")?)?;
            }
            Some("--artifact-root") => {
                artifact_root = PathBuf::from(required_os(&mut args, "--artifact-root")?);
            }
            Some("--help" | "-h") => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unknown argument {}\n{}",
                        Path::new(&arg).display(),
                        usage()
                    ),
                ));
            }
        }
    }

    let command: Vec<_> = args.collect();
    if command.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage()));
    }
    Ok(Config {
        phase: phase.unwrap_or_else(|| "command".to_owned()),
        timeout,
        artifact_root,
        command,
    })
}

fn usage() -> &'static str {
    "usage: nefor-test-watchdog [--phase NAME] [--timeout-seconds N] [--artifact-root PATH] -- COMMAND [ARG ...]"
}

fn required_os(args: &mut impl Iterator<Item = OsString>, option: &str) -> io::Result<OsString> {
    args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} requires a value"),
        )
    })
}

fn required_utf8(args: &mut impl Iterator<Item = OsString>, option: &str) -> io::Result<String> {
    required_os(args, option)?.into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} requires UTF-8"),
        )
    })
}

fn parse_timeout(raw: &str) -> io::Result<Duration> {
    let seconds: f64 = raw.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid timeout seconds: {raw}"),
        )
    })?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout must be a positive finite number",
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn repository_root() -> io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("watchdog manifest is not beneath repository root"))
}

fn run(config: &Config) -> io::Result<RunResult> {
    fs::create_dir_all(&config.artifact_root)?;
    let artifact_dir = unique_artifact_dir(&config.artifact_root, &config.phase)?;
    fs::create_dir(&artifact_dir)?;

    let command_display = format!("{:?}", config.command);
    fs::write(
        artifact_dir.join("command.argv.txt"),
        format!("{command_display}\n"),
    )?;
    let started = Instant::now();
    eprintln!("=== WATCHDOG PHASE START: {} ===", config.phase);
    eprintln!("command argv: {command_display}");
    eprintln!("deadline: {:.3}s", config.timeout.as_secs_f64());
    eprintln!("artifacts: {}", artifact_dir.display());

    let mut command = Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .current_dir(repository_root()?)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: this async-signal-safe libc call runs after fork and before exec.
    unsafe {
        command.pre_exec(|| {
            if setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pgid = i32::try_from(child.id()).map_err(io::Error::other)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    let stdout_thread = tee(stdout, artifact_dir.join("stdout.log"), false);
    let stderr_thread = tee(stderr, artifact_dir.join("stderr.log"), true);

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= config.timeout {
            timed_out = true;
            eprintln!(
                "=== WATCHDOG TIMEOUT: {} after {:.3}s ===",
                config.phase,
                started.elapsed().as_secs_f64()
            );
            capture_diagnostics(&artifact_dir, child.id(), pgid, config, started.elapsed());
            break terminate_and_reap(&mut child, pgid)?;
        }
        thread::sleep(Duration::from_millis(20));
    };

    if !timed_out {
        terminate_remaining_group(pgid);
    }
    join_tee(stdout_thread)?;
    join_tee(stderr_thread)?;

    let outcome = if timed_out {
        "timeout"
    } else if status.success() {
        "success"
    } else {
        "failure"
    };
    let elapsed = started.elapsed();
    fs::write(
        artifact_dir.join("result.txt"),
        format!(
            "outcome={outcome}\nexit_code={:?}\nsignal={:?}\nelapsed_seconds={:.3}\npid={}\npgid={pgid}\n",
            status.code(),
            status.signal(),
            elapsed.as_secs_f64(),
            child.id()
        ),
    )?;
    eprintln!(
        "=== WATCHDOG PHASE END: {} outcome={} elapsed={:.3}s ===",
        config.phase,
        outcome,
        elapsed.as_secs_f64()
    );
    eprintln!("artifacts: {}", artifact_dir.display());

    Ok(RunResult {
        status,
        timed_out,
        #[cfg(test)]
        artifact_dir,
    })
}

fn unique_artifact_dir(root: &Path, phase: &str) -> io::Result<PathBuf> {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let safe_phase: String = phase
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    for suffix in 0..100_u8 {
        let path = root.join(format!(
            "{epoch_millis}-{safe_phase}-{}-{suffix}",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate artifact directory",
    ))
}

fn tee(
    mut input: impl Read + Send + 'static,
    log_path: PathBuf,
    to_stderr: bool,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        let mut log = File::create(log_path)?;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            log.write_all(&buffer[..read])?;
            log.flush()?;
            if to_stderr {
                let mut terminal = io::stderr().lock();
                terminal.write_all(&buffer[..read])?;
                terminal.flush()?;
            } else {
                let mut terminal = io::stdout().lock();
                terminal.write_all(&buffer[..read])?;
                terminal.flush()?;
            }
        }
        Ok(())
    })
}

fn join_tee(handle: thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    handle
        .join()
        .map_err(|_| io::Error::other("output capture thread panicked"))?
}

fn capture_diagnostics(
    artifact_dir: &Path,
    leader_pid: u32,
    group_id: i32,
    config: &Config,
    elapsed: Duration,
) {
    best_effort_write(
        &artifact_dir.join("timeout.txt"),
        format!(
            "phase={}\ntimeout_seconds={:.3}\nelapsed_seconds={:.3}\npid={leader_pid}\npgid={group_id}\ncommand={:?}\n",
            config.phase,
            config.timeout.as_secs_f64(),
            elapsed.as_secs_f64(),
            config.command
        )
        .as_bytes(),
    );

    let ps = diagnostic_command("ps", &["-axo", "pid=,ppid=,pgid=,stat=,etime=,command="]);
    best_effort_write(&artifact_dir.join("process-snapshot.txt"), &ps);
    let pids = pids_in_group(&ps, group_id);
    best_effort_write(
        &artifact_dir.join("group-pids.txt"),
        format!("pid={leader_pid}\npgid={group_id}\nmembers={pids:?}\n").as_bytes(),
    );

    if command_available("lsof") {
        let joined = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = diagnostic_command("lsof", &["-n", "-P", "-p", &joined]);
        best_effort_write(&artifact_dir.join("open-files.txt"), &output);
    } else {
        best_effort_write(&artifact_dir.join("open-files.txt"), b"lsof unavailable\n");
    }

    #[cfg(target_os = "macos")]
    capture_samples(artifact_dir, &pids);
    #[cfg(not(target_os = "macos"))]
    best_effort_write(
        &artifact_dir.join("sample-unavailable.txt"),
        b"macOS sample is not available on this platform\n",
    );
}

fn pids_in_group(snapshot: &[u8], group_id: i32) -> Vec<u32> {
    String::from_utf8_lossy(snapshot)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let process_id = fields.next()?.parse().ok()?;
            let _ppid = fields.next()?;
            let candidate_pgid: i32 = fields.next()?.parse().ok()?;
            (candidate_pgid == group_id).then_some(process_id)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn capture_samples(artifact_dir: &Path, pids: &[u32]) {
    if !command_available("sample") {
        best_effort_write(
            &artifact_dir.join("sample-unavailable.txt"),
            b"sample unavailable\n",
        );
        return;
    }
    for pid in pids {
        let output_path = artifact_dir.join(format!("sample-{pid}.txt"));
        let output_string = output_path.to_string_lossy().into_owned();
        let result = diagnostic_command(
            "sample",
            &[&pid.to_string(), "1", "1", "-file", &output_string],
        );
        if !result.is_empty() {
            best_effort_write(
                &artifact_dir.join(format!("sample-{pid}.command.txt")),
                &result,
            );
        }
    }
}

fn diagnostic_command(program: &str, args: &[&str]) -> Vec<u8> {
    match Command::new(program).args(args).output() {
        Ok(output) => {
            let mut combined = output.stdout;
            combined.extend_from_slice(&output.stderr);
            combined
        }
        Err(error) => format!("{program} unavailable or failed to start: {error}\n").into_bytes(),
    }
}

fn command_available(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", "command -v \"$1\" >/dev/null 2>&1", "sh", program])
        .status()
        .is_ok_and(|status| status.success())
}

fn best_effort_write(path: &Path, content: &[u8]) {
    if let Err(error) = fs::write(path, content) {
        eprintln!("watchdog diagnostic warning ({}): {error}", path.display());
    }
}

fn terminate_and_reap(child: &mut Child, pgid: i32) -> io::Result<ExitStatus> {
    signal_group(pgid, SIGTERM);
    if let Some(status) = wait_bounded(child, TERM_GRACE)? {
        signal_group(pgid, SIGKILL);
        return Ok(status);
    }
    signal_group(pgid, SIGKILL);
    if let Some(status) = wait_bounded(child, KILL_GRACE)? {
        return Ok(status);
    }
    child.wait()
}

fn terminate_remaining_group(pgid: i32) {
    if group_exists(pgid) {
        eprintln!("watchdog: command exited with live group members; terminating them");
        signal_group(pgid, SIGTERM);
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline && group_exists(pgid) {
            thread::sleep(Duration::from_millis(20));
        }
        if group_exists(pgid) {
            signal_group(pgid, SIGKILL);
        }
    }
}

fn wait_bounded(child: &mut Child, duration: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + duration;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: negative pid addresses the owned process group; failures are best-effort.
    unsafe {
        kill(-pgid, signal);
    }
}

fn group_exists(pgid: i32) -> bool {
    // SAFETY: signal zero checks process-group existence without delivering a signal.
    unsafe { kill(-pgid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(name: &str) -> PathBuf {
        let root = repository_root()
            .unwrap()
            .join("tmp/test-watchdog-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn config(root: &Path, phase: &str, timeout: f64, script: &str) -> Config {
        Config {
            phase: phase.to_owned(),
            timeout: Duration::from_secs_f64(timeout),
            artifact_root: root.to_path_buf(),
            command: vec!["sh".into(), "-c".into(), script.into()],
        }
    }

    #[test]
    fn success_preserves_output_and_status() {
        let root = temp_root("success");
        let result = run(&config(&root, "success", 2.0, "printf out; printf err >&2")).unwrap();
        assert!(result.status.success());
        assert!(!result.timed_out);
        assert_eq!(
            fs::read_to_string(result.artifact_dir.join("stdout.log")).unwrap(),
            "out"
        );
        assert_eq!(
            fs::read_to_string(result.artifact_dir.join("stderr.log")).unwrap(),
            "err"
        );
    }

    #[test]
    fn nonzero_exit_is_not_a_timeout() {
        let root = temp_root("failure");
        let result = run(&config(&root, "failure", 2.0, "printf failed >&2; exit 37")).unwrap();
        assert_eq!(result.status.code(), Some(37));
        assert!(!result.timed_out);
        assert!(!result.artifact_dir.join("timeout.txt").exists());
    }

    #[test]
    fn timeout_captures_diagnostics() {
        let root = temp_root("timeout");
        let result = run(&config(&root, "timeout", 0.1, "sleep 30")).unwrap();
        assert!(result.timed_out);
        for name in [
            "timeout.txt",
            "process-snapshot.txt",
            "group-pids.txt",
            "open-files.txt",
            "stdout.log",
            "stderr.log",
        ] {
            assert!(result.artifact_dir.join(name).exists(), "missing {name}");
        }
        #[cfg(target_os = "macos")]
        assert!(
            fs::read_dir(&result.artifact_dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with("sample-")),
            "expected sample evidence or an explicit unavailable marker"
        );
    }

    #[test]
    fn timeout_terminates_descendant_process_group() {
        let root = temp_root("descendants");
        let pid_file = root.join("descendant.pid");
        let fixture = root.join("fixture.sh");
        fs::write(
            &fixture,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o755)).unwrap();
        let result = run(&Config {
            phase: "descendants".to_owned(),
            timeout: Duration::from_millis(200),
            artifact_root: root.clone(),
            command: vec![fixture.into_os_string()],
        })
        .unwrap();
        assert!(result.timed_out);
        let descendant: i32 = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(descendant) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_exists(descendant),
            "descendant {descendant} survived"
        );
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal zero checks existence without delivering a signal.
        unsafe { kill(pid, 0) == 0 }
    }
}
