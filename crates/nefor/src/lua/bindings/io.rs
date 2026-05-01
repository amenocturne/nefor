//! `nefor.io.read_line` — blocking stdin read, CLI-dispatch only.
//!
//! In CLI dispatch mode the engine binary's stdin becomes the input
//! channel for the cli function. The plumbing:
//!
//! 1. [`spawn_stdin_pump`] starts a `tokio::task::spawn_blocking` that
//!    reads `std::io::stdin()` line-by-line and forwards each line into
//!    a tokio mpsc channel. This task lives outside any Lua-VM lock,
//!    so lines flow into the channel even while the VM is busy.
//! 2. The Rust side of `nefor.io.read_line` calls
//!    `Handle::current().block_on(rx.recv())` to wait for the next
//!    line. The Lua VM is held during the block, so no other Lua
//!    callable runs in parallel — same as any blocking C function.
//!    The pump task is on a separate worker thread; the channel
//!    delivers regardless.
//! 3. In TUI mode (the default) the host short-circuits the binding
//!    to return Lua `nil` immediately — no pump runs and `read_line`
//!    is effectively unavailable. Callers are expected to gate use
//!    behind a CLI-mode check.
//!
//! Per D-16 the mode is an enum on the host, not a boolean flag.

use std::io::BufRead;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};
use tokio::sync::mpsc;

use crate::lua::mode::EngineMode;

/// Receiver end of the stdin pump. `Some` when a pump is running (CLI
/// dispatch mode), `None` otherwise. Wrapped so the binding can take
/// `&mut self` access to `recv` under a sync mutex without contesting
/// the Lua VM's own internal mutex.
#[derive(Default)]
pub struct StdinPump {
    rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Sticky EOF flag. Once stdin closes, every subsequent read_line
    /// call returns nil — no busy-looping on a closed channel.
    eof: bool,
}

impl StdinPump {
    /// Construct an empty (no-pump) state. Used in TUI mode.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Replace the contained receiver with a fresh one. Idempotent.
    pub fn set_rx(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.rx = Some(rx);
        self.eof = false;
    }

    /// True when the receiver was never set (TUI mode) or stdin EOF was
    /// observed.
    pub fn is_eof(&self) -> bool {
        self.rx.is_none() || self.eof
    }
}

/// Shared handle alias.
pub type SharedStdinPump = Arc<Mutex<StdinPump>>;

/// Spawn the stdin pump. Returns the receiver to be passed to a host
/// for installation, plus a join handle the caller may discard. The
/// pump uses [`tokio::task::spawn_blocking`] because `std::io::stdin`'s
/// line-buffered read is a synchronous syscall.
pub fn spawn_stdin_pump() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let lock = stdin.lock();
        for line in lock.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "stdin read error; closing pump");
                    break;
                }
            }
        }
        // Channel drops when this returns → receiver gets None → read_line
        // returns nil for every subsequent call (sticky EOF).
    });
    rx
}

/// Install `nefor.io.read_line` onto `nefor_tbl`.
///
/// `mode` chooses the dispatch shape: TUI returns nil immediately.
/// `pump` provides the stdin source in CLI dispatch mode.
pub fn install_io(
    lua: &Lua,
    nefor_tbl: &Table,
    mode: EngineMode,
    pump: SharedStdinPump,
) -> mlua::Result<()> {
    let io_tbl = lua.create_table()?;

    let read_line = lua.create_function(move |lua, _: ()| -> mlua::Result<Value> {
        // TUI mode: stdin is unused at the engine level (plugins own
        // their subprocess stdin), so return nil immediately rather than
        // blocking on a channel nobody pumps.
        if !matches!(mode, EngineMode::PluginDispatch { .. }) {
            return Ok(Value::Nil);
        }

        // CLI dispatch mode: take the receiver out under the lock,
        // recv on it, put it back. recv is awaited via the current
        // tokio runtime handle — block_in_place keeps the runtime
        // healthy if we're on a worker (the pump task can still run).
        let mut guard = match pump.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_eof() {
            return Ok(Value::Nil);
        }
        let mut rx = match guard.rx.take() {
            Some(rx) => rx,
            None => return Ok(Value::Nil),
        };
        // Drop the guard before blocking so a concurrent set_rx
        // (unlikely but possible during shutdown) can proceed.
        drop(guard);

        let handle = tokio::runtime::Handle::current();
        let recv = tokio::task::block_in_place(|| handle.block_on(rx.recv()));

        // Put the receiver back regardless of recv outcome so the next
        // call sees the same channel.
        let mut guard = match pump.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match recv {
            Some(line) => {
                guard.rx = Some(rx);
                Ok(Value::String(lua.create_string(&line)?))
            }
            None => {
                // EOF: drop the rx and mark sticky-EOF so we don't
                // re-block on a closed channel.
                guard.rx = None;
                guard.eof = true;
                Ok(Value::Nil)
            }
        }
    })?;
    io_tbl.set("read_line", read_line)?;

    nefor_tbl.set("io", io_tbl)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pattern_empty_pump_is_eof() {
        let p = StdinPump::empty();
        assert!(p.is_eof());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_line_returns_nil_in_tui_mode() {
        let lua = Lua::new();
        let pump: SharedStdinPump = Arc::new(Mutex::new(StdinPump::empty()));
        let nefor = lua.create_table().unwrap();
        install_io(&lua, &nefor, EngineMode::Tui, Arc::clone(&pump)).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        let v: Value = lua
            .load(r#"return nefor.io.read_line()"#)
            .eval()
            .expect("eval ok");
        assert!(matches!(v, Value::Nil));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_line_returns_nil_when_pump_unset_in_cli_mode() {
        let lua = Lua::new();
        let pump: SharedStdinPump = Arc::new(Mutex::new(StdinPump::empty()));
        let nefor = lua.create_table().unwrap();
        install_io(
            &lua,
            &nefor,
            EngineMode::PluginDispatch {
                name: "x".into(),
                args: vec![],
            },
            Arc::clone(&pump),
        )
        .unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        let v: Value = lua
            .load(r#"return nefor.io.read_line()"#)
            .eval()
            .expect("eval ok");
        assert!(matches!(v, Value::Nil));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_line_reads_from_channel() {
        let lua = Lua::new();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let pump: SharedStdinPump = Arc::new(Mutex::new(StdinPump::empty()));
        pump.lock().unwrap().set_rx(rx);

        let nefor = lua.create_table().unwrap();
        install_io(
            &lua,
            &nefor,
            EngineMode::PluginDispatch {
                name: "x".into(),
                args: vec![],
            },
            Arc::clone(&pump),
        )
        .unwrap();
        lua.globals().set("nefor", nefor).unwrap();

        tx.send("hello".to_string()).unwrap();
        drop(tx); // EOF after one line.

        // First read returns the line.
        let s: String = lua
            .load(r#"return nefor.io.read_line()"#)
            .eval()
            .expect("eval ok");
        assert_eq!(s, "hello");
        // Second read sees EOF → nil.
        let v: Value = lua
            .load(r#"return nefor.io.read_line()"#)
            .eval()
            .expect("eval ok");
        assert!(matches!(v, Value::Nil));
        // Sticky EOF: third read still nil without reblocking.
        let v: Value = lua
            .load(r#"return nefor.io.read_line()"#)
            .eval()
            .expect("eval ok");
        assert!(matches!(v, Value::Nil));
    }
}
