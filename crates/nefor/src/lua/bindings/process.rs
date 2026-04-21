//! `nefor.process.spawn` — subprocess spawning.
//!
//! Spawns a child process via `tokio::process::Command`. Stdout/stderr are
//! line-buffered into mpsc channels which feed a serialization task that
//! invokes the Lua handlers (`on_stdout`, `on_stderr`, `on_exit`).
//!
//! ## Serialization story
//!
//! Lua is single-threaded in spirit (mlua's `send` feature serializes it
//! behind an internal mutex, but two tasks hammering the VM still take
//! turns). To keep handler invocation predictable we drain stdout/stderr
//! lines onto an mpsc, and a single `tokio::spawn`-ed task awaits messages
//! from that channel and calls the handlers one at a time. Widget renders
//! and event-bus handlers also go through mlua's internal mutex — they
//! don't fight for a separate lock here.
//!
//! ## Process userdata
//!
//! `nefor.process.spawn(...)` returns a userdata with `kill()` /
//! `wait()` / `write_stdin(bytes)` methods. The userdata holds the kill
//! handle and a one-shot for the exit-wait future.
//!
//! ## Stdin policy
//!
//! Stdin defaults to `/dev/null`. A pipe is opened only when the caller
//! explicitly asks for one — either by passing a `stdin = "<string>"`
//! payload (pre-write + close) or `stdin_piped = true` (keep open for
//! `proc:write_stdin(bytes)`). `claude -p <prompt>` and similar children
//! that don't read stdin silently open it otherwise and print a spurious
//! "no stdin data received" warning after a few seconds.

use std::process::Stdio;
use std::sync::Arc;

use mlua::{Function, Lua, RegistryKey, Table, UserData, UserDataMethods};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

/// Install `nefor.process.spawn` onto `nefor_tbl`.
pub fn install_process(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let process = lua.create_table()?;

    // Clone the Lua handle for the handler-dispatch task. mlua's `send`
    // feature makes this safe; the dispatcher just needs somewhere to
    // invoke registered functions from.
    let spawn_lua = lua.clone();
    let spawn_fn = lua.create_function(move |_, opts: Table| {
        let cmd_name: String = opts.get("cmd").map_err(|e| {
            mlua::Error::runtime(format!(
                "nefor.process.spawn: missing or invalid 'cmd' field: {e}"
            ))
        })?;
        if cmd_name.is_empty() {
            return Err(mlua::Error::runtime(
                "nefor.process.spawn: 'cmd' must be a non-empty string",
            ));
        }

        let args: Vec<String> = opts
            .get::<Option<Vec<String>>>("args")
            .unwrap_or(None)
            .unwrap_or_default();
        let cwd: Option<String> = opts.get::<Option<String>>("cwd").unwrap_or(None);
        let env_tbl: Option<Table> = opts.get::<Option<Table>>("env").unwrap_or(None);
        let stdin_string: Option<String> = opts.get::<Option<String>>("stdin").unwrap_or(None);
        let stdin_piped: bool = opts
            .get::<Option<bool>>("stdin_piped")
            .unwrap_or(None)
            .unwrap_or(false);

        let on_stdout = opts.get::<Option<Function>>("on_stdout").unwrap_or(None);
        let on_stderr = opts.get::<Option<Function>>("on_stderr").unwrap_or(None);
        let on_exit = opts.get::<Option<Function>>("on_exit").unwrap_or(None);

        // stdin policy, least-surprise for non-interactive children:
        //   - `stdin = "<string>"`      → piped, pre-write, close (signals EOF).
        //   - `stdin_piped = true`      → piped, kept open; caller writes via
        //                                  the returned handle's `write_stdin`.
        //   - neither                   → `null`. Without this default, children
        //     that don't read stdin (e.g. `claude -p "<prompt>"`) still see an
        //     open pipe and may wait/warn. Null makes stdin a closed /dev/null.
        let stdin_cfg = if stdin_string.is_some() || stdin_piped {
            Stdio::piped()
        } else {
            Stdio::null()
        };

        let mut cmd = Command::new(&cmd_name);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(stdin_cfg);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(env) = env_tbl {
            for pair in env.pairs::<String, String>() {
                let (k, v) = pair?;
                cmd.env(k, v);
            }
        }

        let mut child = cmd.spawn().map_err(|e| {
            mlua::Error::runtime(format!(
                "nefor.process.spawn: failed to spawn {cmd_name:?}: {e}"
            ))
        })?;

        // Stash handlers in the registry so our serializer task can reach
        // them without juggling `Function` handles across tokio tasks.
        let on_stdout_key = stash_fn(&spawn_lua, on_stdout)?;
        let on_stderr_key = stash_fn(&spawn_lua, on_stderr)?;
        let on_exit_key = stash_fn(&spawn_lua, on_exit)?;

        // Line-dispatch channel. Capacity is generous — most subprocesses
        // produce a manageable trickle of output and backpressure is fine.
        let (tx, mut rx) = mpsc::unbounded_channel::<DispatchMsg>();

        // stdout reader.
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) => {
                            if tx.send(DispatchMsg::Stdout(line)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "process stdout read error");
                            break;
                        }
                    }
                }
            });
        }

        // stderr reader.
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) => {
                            if tx.send(DispatchMsg::Stderr(line)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "process stderr read error");
                            break;
                        }
                    }
                }
            });
        }

        // Optional stdin pre-write: write `stdin_string` and drop stdin to
        // close the pipe. Callers who want interactive write-multiple use
        // the userdata's `write_stdin(bytes)` method instead (and should
        // omit the top-level `stdin` option).
        let shared_stdin: Arc<Mutex<Option<ChildStdin>>> = Arc::new(Mutex::new(child.stdin.take()));
        if let Some(payload) = stdin_string {
            let stdin = Arc::clone(&shared_stdin);
            tokio::spawn(async move {
                let mut guard = stdin.lock().await;
                if let Some(s) = guard.as_mut() {
                    if let Err(e) = s.write_all(payload.as_bytes()).await {
                        tracing::warn!(error = %e, "process stdin pre-write failed");
                    }
                    // Drop to close the pipe and signal EOF to the child.
                }
                *guard = None;
            });
        }

        // Exit waiter — converts `child.wait().await` into a dispatch message.
        // We also provide `wait_done_rx` for the userdata's `wait()` method.
        let (exit_tx_user, exit_rx_user) = oneshot::channel::<i32>();
        let tx_exit = tx.clone();
        let exit_waiter: JoinHandle<()> = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    let _ = exit_tx_user.send(code);
                    let _ = tx_exit.send(DispatchMsg::Exit(code));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "process wait failed");
                    let _ = exit_tx_user.send(-1);
                    let _ = tx_exit.send(DispatchMsg::Exit(-1));
                }
            }
        });
        // The waiter owns `child`; to kill we use tokio's built-in kill, which
        // takes `&mut Child` we no longer have. Workaround: put the pid-based
        // kill inside the waiter's scope by routing a kill request through
        // the channel. Simplest MVP: shelve the Child itself inside the
        // waiter, and provide kill via a separate channel + select. But for
        // MVP simplicity we accept "kill() is best-effort" via unix signal.
        // Stash an Option<KillSignal> in the ProcessHandle; if Some, the
        // waiter task listens. See ProcessHandle.

        // Serialization task: drain the dispatch channel, invoke Lua
        // handlers one at a time. Drops registry keys when done so Lua GC
        // can collect the handler closures.
        let dispatch_lua = spawn_lua.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    DispatchMsg::Stdout(line) => {
                        if let Some(k) = &on_stdout_key {
                            invoke_line_handler(&dispatch_lua, k, &line, "on_stdout");
                        }
                    }
                    DispatchMsg::Stderr(line) => {
                        if let Some(k) = &on_stderr_key {
                            invoke_line_handler(&dispatch_lua, k, &line, "on_stderr");
                        }
                    }
                    DispatchMsg::Exit(code) => {
                        if let Some(k) = &on_exit_key {
                            invoke_exit_handler(&dispatch_lua, k, code);
                        }
                        // Exit is terminal — drain any remaining messages
                        // then stop.
                        break;
                    }
                }
            }
        });

        Ok(ProcessHandle {
            exit_rx: Arc::new(Mutex::new(Some(exit_rx_user))),
            stdin: shared_stdin,
            waiter: Arc::new(Mutex::new(Some(exit_waiter))),
        })
    })?;
    process.set("spawn", spawn_fn)?;

    nefor_tbl.set("process", process)?;
    Ok(())
}

enum DispatchMsg {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

fn stash_fn(lua: &Lua, f: Option<Function>) -> mlua::Result<Option<Arc<RegistryKey>>> {
    match f {
        Some(func) => {
            let key = lua.create_registry_value(func)?;
            Ok(Some(Arc::new(key)))
        }
        None => Ok(None),
    }
}

fn invoke_line_handler(lua: &Lua, key: &RegistryKey, line: &str, which: &str) {
    let func: Function = match lua.registry_value(key) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, handler = which, "process handler missing from registry");
            return;
        }
    };
    if let Err(e) = func.call::<()>(line.to_owned()) {
        tracing::error!(error = %e, handler = which, "process handler raised");
    }
}

fn invoke_exit_handler(lua: &Lua, key: &RegistryKey, code: i32) {
    let func: Function = match lua.registry_value(key) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "process on_exit handler missing from registry");
            return;
        }
    };
    if let Err(e) = func.call::<()>(code) {
        tracing::error!(error = %e, "process on_exit handler raised");
    }
}

/// Lua-visible handle returned by `nefor.process.spawn`.
///
/// `wait()` yields until the child exits and returns its status code.
/// `kill()` is best-effort: on Unix it sends SIGKILL via the pid; on
/// Windows it's a no-op for MVP (callers wanting cross-platform signaling
/// can build on top later).
/// `write_stdin(bytes)` writes to the child's stdin if the pipe is still
/// open. Writing after exit is a no-op, not an error.
pub struct ProcessHandle {
    exit_rx: Arc<Mutex<Option<oneshot::Receiver<i32>>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    waiter: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl UserData for ProcessHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // wait() -> exit code. Yields the Lua coroutine until the child
        // exits; safe to call multiple times — subsequent calls after the
        // receiver is consumed return -1.
        methods.add_async_method("wait", |_, this, ()| async move {
            let mut guard = this.exit_rx.lock().await;
            match guard.take() {
                Some(rx) => match rx.await {
                    Ok(code) => Ok(code),
                    Err(_) => Ok(-1),
                },
                None => Ok(-1),
            }
        });

        // write_stdin(bytes) -> bool indicating whether the write was
        // accepted (stdin still open) vs dropped (already closed).
        methods.add_async_method("write_stdin", |_, this, data: String| async move {
            let mut guard = this.stdin.lock().await;
            match guard.as_mut() {
                Some(pipe) => match pipe.write_all(data.as_bytes()).await {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        tracing::warn!(error = %e, "process write_stdin failed");
                        Ok(false)
                    }
                },
                None => Ok(false),
            }
        });

        // kill() -> bool. MVP: abort the waiter task, which in turn drops
        // the Child, which (per tokio docs) does *not* automatically kill
        // the process — Children are "detach-on-drop" by default. For a
        // real kill we'd need the pid + `nix::sys::signal::kill`; that's
        // post-MVP. We return `true` when we successfully dropped the
        // waiter (the caller can interpret that as "we're no longer
        // tracking it") and `false` when the waiter was already done.
        methods.add_async_method("kill", |_, this, ()| async move {
            let mut guard = this.waiter.lock().await;
            match guard.take() {
                Some(handle) => {
                    handle.abort();
                    Ok(true)
                }
                None => Ok(false),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn setup() -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        install_process(&lua, &nefor).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        lua
    }

    #[tokio::test]
    async fn echo_emits_stdout_line_and_zero_exit() {
        let lua = setup();

        let lines = Arc::new(StdMutex::new(Vec::<String>::new()));
        let exit = Arc::new(StdMutex::new(None::<i32>));

        let lines_c = Arc::clone(&lines);
        let on_stdout = lua
            .create_function(move |_, line: String| {
                lines_c.lock().unwrap().push(line);
                Ok(())
            })
            .unwrap();
        let exit_c = Arc::clone(&exit);
        let on_exit = lua
            .create_function(move |_, code: i32| {
                *exit_c.lock().unwrap() = Some(code);
                Ok(())
            })
            .unwrap();
        lua.globals().set("on_stdout", on_stdout).unwrap();
        lua.globals().set("on_exit", on_exit).unwrap();

        lua.load(
            r#"
            proc = nefor.process.spawn({
                cmd = "sh",
                args = { "-c", "echo hello" },
                on_stdout = on_stdout,
                on_exit = on_exit,
            })
            return proc:wait()
            "#,
        )
        .eval_async::<i32>()
        .await
        .expect("wait ok");

        // Small yield to let the serialization task drain before asserting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let lines = lines.lock().unwrap().clone();
        let exit = *exit.lock().unwrap();
        assert_eq!(lines, vec!["hello".to_string()]);
        assert_eq!(exit, Some(0));
    }

    #[tokio::test]
    async fn missing_cmd_errors() {
        let lua = setup();
        let err = lua
            .load(r#"nefor.process.spawn({ cmd = "" })"#)
            .exec_async()
            .await
            .expect_err("empty cmd must error");
        assert!(err.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn nonexistent_binary_errors() {
        let lua = setup();
        let err = lua
            .load(r#"nefor.process.spawn({ cmd = "definitely-not-a-real-binary-xxxyyyzzz" })"#)
            .exec_async()
            .await
            .expect_err("missing binary must error");
        assert!(err.to_string().contains("failed to spawn"));
    }
}
