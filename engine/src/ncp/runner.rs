//! Plugin runner — spawns Lua-declared subprocesses and bridges stdio.
//!
//! The runner executes `command[0]` directly with `command[1..]` as argv.
//! It does not invoke a shell, infer installation layout, or add per-plugin
//! cwd/environment policy. Children inherit the engine cwd and environment.

use std::process::Stdio;

use tokio::process::Command;

use crate::ncp::error::BrokerError;
use crate::ncp::spawn::PluginSpec;
use crate::ncp::transport::{stdio_transport, ExitOutcome, Transport};

/// Spawn the subprocess explicitly declared by `spec`.
///
/// The caller filters virtual specs (`spec.command().is_none()`) before
/// calling. The runner rejects them rather than guessing what to spawn.
pub fn spawn_plugin(spec: &PluginSpec) -> Result<Transport, BrokerError> {
    let command = spec.command().ok_or_else(|| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: Vec::new(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "spec has no command (virtual plugin must not be subprocess-spawned)",
        ),
    })?;

    let (binary, args) = command.split_first().ok_or_else(|| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: command.to_vec(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command array"),
    })?;

    let mut cmd = Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|source| BrokerError::Spawn {
        name: spec.name.as_str().to_owned(),
        command: command.to_vec(),
        source,
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io_err("child stdin missing"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io_err("child stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io_err("child stderr missing"))?;

    let exit = Box::pin(async move {
        match child.wait().await {
            Ok(status) if status.success() => ExitOutcome::CleanExit,
            Ok(status) => exit_outcome(status),
            Err(error) => ExitOutcome::Unknown {
                reason: error.to_string(),
            },
        }
    });

    Ok(stdio_transport(stdin, stdout, stderr, exit))
}

fn exit_outcome(status: std::process::ExitStatus) -> ExitOutcome {
    if let Some(code) = status.code() {
        return ExitOutcome::ExitCode(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return ExitOutcome::Signal(signal);
        }
    }

    ExitOutcome::Crash
}

fn io_err(msg: &str) -> BrokerError {
    BrokerError::Io(std::io::Error::other(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::PluginName;

    #[tokio::test]
    async fn spawn_plugin_executes_declared_command_without_a_layout_root() {
        let spec = PluginSpec {
            name: PluginName::new("echo-plugin").expect("valid"),
            kind: crate::ncp::spawn::PluginKind::Command(vec!["echo".into()]),
        };
        assert!(spawn_plugin(&spec).is_ok());
    }

    #[test]
    fn spawn_plugin_rejects_virtual_spec() {
        let spec = PluginSpec {
            name: PluginName::new("virtual").expect("valid"),
            kind: crate::ncp::spawn::PluginKind::Cli,
        };
        match spawn_plugin(&spec) {
            Err(BrokerError::Spawn { source, .. }) => {
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
            }
            Err(other) => panic!("expected Spawn err for virtual spec, got {other:?}"),
            Ok(_) => panic!("expected error for virtual spec"),
        }
    }
}
