//! Embedded Lua VM that hosts the MAG kernel.
//!
//! The kernel proper (actor inventory, ready barrier, mailboxes, routing,
//! the fold over graph modifications) is Lua-resident — see
//! `plugins/mag/docs/actor-model.md` and `docs/ir.md`. This module is the
//! Rust host: it creates the VM, installs the native surface the kernel
//! needs (log, json, fs, a millisecond clock, and a bus-emit queue), loads
//! the kernel entry file, and drives the kernel's execute seams
//! (`begin_run`, `start`, `poll`, `bus_response`) from the plugin's dispatch
//! loop.
//!
//! The bus seam is a queue, not an async callback: the kernel modules call
//! `nefor.emit` synchronously from inside the fold (never a coroutine), so
//! emitted bodies land on an in-VM array that the host drains after each
//! kernel call and forwards to the NCP writer. This mirrors the nefor-tui
//! plugin's emit-drain pattern.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::{Function, Lua, LuaSerdeExt, Table, Value};
use serde_json::{Map, Value as JsonValue};

use crate::error::MagError;

/// Global Lua array `nefor.emit` appends to; drained by [`LuaHost::drain_emits`].
const EMIT_QUEUE: &str = "__mag_emit_queue";

/// Named-registry slot holding the in-flight ready-barrier handle across polls.
const BARRIER_SLOT: &str = "mag_barrier";

/// The settled state of a ready barrier (`starter/mag-kernel/barrier.lua`).
#[derive(Debug, Clone)]
pub struct BarrierState {
    pub done: bool,
    pub ok: bool,
    pub error: Option<String>,
}

/// A run's terminal signal, surfaced from the sink through the kernel
/// (`mag.run_complete`). Carries the sink's final result INLINE plus the
/// persisted output path: the terminal reply is the one place the control
/// plane consumes the result itself (the lead relays it to the model), so
/// the result rides here; everything mid-run stays paths-only (docs/ir.md).
#[derive(Debug, Clone)]
pub struct RunCompletion {
    pub output_path: Option<String>,
    pub persisted: bool,
    pub result: Option<JsonValue>,
}

/// Owns the Lua VM and the kernel table it produced.
///
/// Kept alive for the whole session: the VM is the kernel's entire world
/// (per the actor model), so dropping it would tear the kernel down.
pub struct LuaHost {
    kernel: Table,
    lua: Lua,
}

impl LuaHost {
    /// Read, evaluate, and hold the kernel at `path`.
    ///
    /// `lua_root` is the composition-owned shared Lua tree (the repo/checkout
    /// `lua/` directory holding `libs/output-persistence` etc.), threaded in
    /// via `--lua-root` argv; `None` falls back to the search in
    /// [`set_kernel_path`].
    ///
    /// The chunk is expected to return a table (the kernel). Anything else is
    /// a [`MagError::KernelNotTable`]. The kernel may call `nefor.log(msg)`
    /// during load; those messages route to the plugin's tracing subscriber
    /// (stderr), never stdout — stdout is the NCP wire.
    pub fn load_kernel(path: &Path, lua_root: Option<&Path>) -> Result<Self, MagError> {
        let source = std::fs::read_to_string(path).map_err(|source| MagError::KernelRead {
            path: path.display().to_string(),
            source,
        })?;

        let lua = Lua::new();
        let data_root = resolve_data_root();
        install_nefor(&lua, data_root.clone())?;
        if let Some(dir) = path.parent() {
            set_kernel_path(&lua, dir, lua_root, &data_root)?;
        }

        let chunk_name = format!("@{}", path.display());
        let value: Value = lua.load(&source).set_name(chunk_name.as_str()).eval()?;

        let kernel = match value {
            Value::Table(t) => t,
            other => {
                return Err(MagError::KernelNotTable {
                    path: path.display().to_string(),
                    got: other.type_name().to_string(),
                })
            }
        };

        let name = kernel_name(&kernel);
        tracing::info!(kernel = %name.as_deref().unwrap_or("<unnamed>"), "mag kernel loaded");

        Ok(LuaHost { kernel, lua })
    }

    /// The kernel table's `name` field, if it exposed one.
    pub fn kernel_name(&self) -> Option<String> {
        kernel_name(&self.kernel)
    }

    /// The names of the factories the kernel's registry knows — the source of
    /// truth the control plane validates reasoner types against (replaces the
    /// lead's hand-synced allowlist). Empty when the kernel predates the
    /// `registry_names` surface.
    pub fn registry_names(&self) -> Result<Vec<String>, MagError> {
        let f: Option<Function> = self.kernel.get("registry_names")?;
        match f {
            Some(f) => Ok(f.call::<Vec<String>>(())?),
            None => Ok(Vec::new()),
        }
    }

    /// Set the run context (session/run identity) and emit `mag.run_started`.
    /// Run identity is injected, never ambient (docs/ir.md).
    pub fn begin_run(
        &self,
        run_id: &str,
        run_name: &str,
        session_id: Option<&str>,
    ) -> Result<(), MagError> {
        let meta = self.lua.create_table()?;
        meta.set("run_id", run_id)?;
        meta.set("run_name", run_name)?;
        if let Some(s) = session_id {
            meta.set("session_id", s)?;
        }
        let f: Function = self.kernel.get("begin_run")?;
        f.call::<()>(meta)?;
        Ok(())
    }

    /// Apply a program's initial modification behind the ready barrier and
    /// stash the returned handle for [`LuaHost::poll`]. Synchronous factories
    /// (the shipped ones) confirm ready in-constructor, so a well-formed static
    /// program releases and runs to completion inside this call.
    pub fn start(
        &self,
        modification: &JsonValue,
        deadline_ms: Option<u64>,
    ) -> Result<BarrierState, MagError> {
        let mod_val = self.lua.to_value(modification)?;
        let opts = self.lua.create_table()?;
        if let Some(d) = deadline_ms {
            opts.set("deadline_ms", d)?;
        }
        let f: Function = self.kernel.get("start")?;
        let handle: Table = f.call::<Table>((mod_val, opts))?;
        let state = barrier_state(&handle)?;
        self.lua.set_named_registry_value(BARRIER_SLOT, handle)?;
        Ok(state)
    }

    /// Apply one graph modification directly through the fold (no ready
    /// barrier). This is the control plane's direct kernel op (docs/ir.md,
    /// "Kernel operations": modifications reach `actors`/`kills`/`messages`
    /// through the fold, and "the control plane reaches them directly"). The
    /// mid-run kill surface uses it: a `{ kills = [...] }` modification unroutes
    /// the target, hands it its final kill message (the factory's abort envelope
    /// reaches the bus), and drops its correlations. Returns the fold's verbatim
    /// `{ ok, error }`.
    pub fn apply(&self, modification: &JsonValue) -> Result<BarrierState, MagError> {
        let mod_val = self.lua.to_value(modification)?;
        let f: Function = self.kernel.get("apply")?;
        let res: Table = f.call::<Table>(mod_val)?;
        Ok(BarrierState {
            done: true,
            ok: res.get::<Option<bool>>("ok")?.unwrap_or(false),
            error: res.get::<Option<String>>("error")?,
        })
    }

    /// Advance the stashed barrier handle against the current clock. Idempotent
    /// once settled. Returns the barrier's settled state (or a "done" no-op
    /// when no run is in flight).
    pub fn poll(&self) -> Result<BarrierState, MagError> {
        let handle: Option<Table> = self.lua.named_registry_value(BARRIER_SLOT)?;
        let handle = match handle {
            Some(h) => h,
            None => {
                return Ok(BarrierState {
                    done: true,
                    ok: true,
                    error: None,
                })
            }
        };
        let f: Function = self.kernel.get("poll")?;
        let handle: Table = f.call::<Table>((handle, Value::Nil))?;
        barrier_state(&handle)
    }

    /// Deliver a correlated capability response (tool.result-shaped) back to the
    /// requesting actor, advancing any deferred activation it unblocks.
    pub fn bus_response(
        &self,
        id: &str,
        result: Option<&JsonValue>,
        error: Option<&str>,
    ) -> Result<(), MagError> {
        let resp = self.lua.create_table()?;
        resp.set("id", id)?;
        if let Some(r) = result {
            resp.set("result", self.lua.to_value(r)?)?;
        }
        if let Some(e) = error {
            resp.set("error", e)?;
        }
        let f: Function = self.kernel.get("bus_response")?;
        f.call::<()>(resp)?;
        Ok(())
    }

    /// Take the last run-completion signal, if the resident run has finished.
    /// One-shot: clears the slot so a subsequent execute starts fresh.
    pub fn take_run_complete(&self) -> Result<Option<RunCompletion>, MagError> {
        let f: Option<Function> = self.kernel.get("take_run_complete")?;
        let f = match f {
            Some(f) => f,
            None => return Ok(None),
        };
        let rc: Option<Table> = f.call::<Option<Table>>(())?;
        match rc {
            None => Ok(None),
            Some(t) => {
                let result = match t.get::<Value>("result")? {
                    Value::Nil => None,
                    v => Some(self.lua.from_value(v)?),
                };
                Ok(Some(RunCompletion {
                    output_path: t.get::<Option<String>>("output_path")?,
                    persisted: t.get::<Option<bool>>("persisted")?.unwrap_or(false),
                    result,
                }))
            }
        }
    }

    /// Take the last unhandled-failure signal, if an actor failure escalated to
    /// a run failure (an unrouted failure tag — routing.lua apply_completion →
    /// `mag.run_failed`). One-shot: clears the slot. Returns the failure detail
    /// the run's terminal reply surfaces.
    pub fn take_run_failed(&self) -> Result<Option<String>, MagError> {
        let f: Option<Function> = self.kernel.get("take_run_failed")?;
        let f = match f {
            Some(f) => f,
            None => return Ok(None),
        };
        let rf: Option<Table> = f.call::<Option<Table>>(())?;
        match rf {
            None => Ok(None),
            Some(t) => {
                let error = t
                    .get::<Option<String>>("error")?
                    .unwrap_or_else(|| "mag run failed".into());
                Ok(Some(error))
            }
        }
    }

    /// Drain everything the kernel emitted since the last drain (bus + lifecycle
    /// events), converting each to an NCP event body. The queue is reset atomically.
    pub fn drain_emits(&self) -> Result<Vec<Map<String, JsonValue>>, MagError> {
        let queue: Table = self.lua.globals().get(EMIT_QUEUE)?;
        let mut out = Vec::new();
        for pair in queue.clone().pairs::<i64, Value>() {
            let (_, v) = pair?;
            let json: JsonValue = self.lua.from_value(v)?;
            if let JsonValue::Object(map) = json {
                out.push(map);
            }
        }
        // Reset to a fresh array so the next drain starts empty.
        self.lua
            .globals()
            .set(EMIT_QUEUE, self.lua.create_table()?)?;
        Ok(out)
    }
}

/// Read `{ done, ok, error }` off a barrier handle table.
fn barrier_state(handle: &Table) -> Result<BarrierState, MagError> {
    Ok(BarrierState {
        done: handle.get::<Option<bool>>("done")?.unwrap_or(false),
        ok: handle.get::<Option<bool>>("ok")?.unwrap_or(false),
        error: handle.get::<Option<String>>("error")?,
    })
}

/// Read the `name` field off a kernel table, tolerating its absence or a
/// non-string value.
fn kernel_name(kernel: &Table) -> Option<String> {
    kernel.get::<Option<String>>("name").ok().flatten()
}

/// Resolve the data root the same way the ecosystem does: `NEFOR_DATA_DIR`,
/// then `XDG_DATA_HOME/nefor`, then `~/.local/share/nefor`. Used for the plugin
/// VM's `nefor.fs.data_root()` so per-node output persistence lands under the
/// same session layout the rest of nefor writes to.
fn resolve_data_root() -> String {
    if let Some(d) = std::env::var_os("NEFOR_DATA_DIR") {
        if !d.is_empty() {
            return d.to_string_lossy().into_owned();
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return format!("{}/nefor", xdg.to_string_lossy());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return format!("{}/.local/share/nefor", home.to_string_lossy());
    }
    String::from("/tmp/nefor")
}

/// Point `package.path` at the kernel file's directory (so the entry chunk can
/// `require` sibling modules by bare name) plus the shared `lua/` and
/// `lua/libs/` trees (so `output-persistence` resolves). Lua trees, highest
/// precedence first:
///
/// 1. `lua_root` — the composition-owned `--lua-root` (starter/init.lua
///    threads its resolved `NEFOR_ROOT/lua` here).
/// 2. `NEFOR_DEV_DIR/lua` — in-checkout dev mode.
/// 3. the kernel dir's grandparent's `lua/` (`.../starter/mag-kernel` →
///    repo root) — covers a bare `--kernel` pointing into a checkout.
/// 4. `<data_root>/nefor/lua` — the pm-managed sparse-clone every installed
///    config bootstraps (starter/init.lua), so an installed kernel whose
///    config dir carries no `lua/` tree still resolves the shared libs.
fn set_kernel_path(
    lua: &Lua,
    dir: &Path,
    lua_root: Option<&Path>,
    data_root: &str,
) -> Result<(), MagError> {
    let package: Table = lua.globals().get("package")?;
    let current: String = package.get("path")?;

    let mut entries: Vec<String> = Vec::new();
    let kdir = dir.display().to_string();
    entries.push(format!("{kdir}/?.lua"));
    entries.push(format!("{kdir}/?/init.lua"));

    let mut trees: Vec<PathBuf> = Vec::new();
    if let Some(root) = lua_root {
        trees.push(root.to_path_buf());
    }
    if let Some(dev) = std::env::var_os("NEFOR_DEV_DIR") {
        if !dev.is_empty() {
            trees.push(PathBuf::from(dev).join("lua"));
        }
    }
    // .../starter/mag-kernel → grandparent is the repo/config root.
    if let Some(root) = dir.parent().and_then(Path::parent) {
        trees.push(root.join("lua"));
    }
    trees.push(PathBuf::from(data_root).join("nefor/lua"));
    for tree in trees {
        for base in [tree.clone(), tree.join("libs")] {
            let base = base.display().to_string();
            entries.push(format!("{base}/?.lua"));
            entries.push(format!("{base}/?/init.lua"));
        }
    }

    entries.push(current);
    package.set("path", entries.join(";"))?;
    Ok(())
}

/// Install the `nefor` global the kernel needs: `log`, `json`, `fs`, a
/// millisecond clock, and the bus-emit queue.
fn install_nefor(lua: &Lua, data_root: String) -> Result<(), MagError> {
    let nefor = lua.create_table()?;

    let log = lua.create_function(|_, msg: String| {
        tracing::info!(target: "mag::kernel", "{msg}");
        Ok(())
    })?;
    nefor.set("log", log)?;

    install_json(lua, &nefor)?;
    install_fs(lua, &nefor, data_root)?;

    let now_ms = lua.create_function(|_, _: ()| {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(ms)
    })?;
    nefor.set("now_ms", now_ms)?;

    // Bus-emit queue. `nefor.emit(body)` appends onto a global array the host
    // drains after each kernel call; the kernel's injected `bus_emit`/
    // `emit_event` seams call it synchronously from inside the fold.
    lua.globals().set(EMIT_QUEUE, lua.create_table()?)?;
    let emit = lua.create_function(|lua, body: Table| {
        let queue: Table = lua.globals().get(EMIT_QUEUE)?;
        let n = queue.raw_len();
        queue.raw_set(n + 1, body)?;
        Ok(())
    })?;
    nefor.set("emit", emit)?;

    lua.globals().set("nefor", nefor)?;
    Ok(())
}

/// `nefor.json.{encode, decode}` over serde_json (mlua serialize bridge).
/// Mirrors the engine's `nefor::lua::bindings::install_json`.
fn install_json(lua: &Lua, nefor_tbl: &Table) -> Result<(), MagError> {
    let json = lua.create_table()?;

    let encode = lua.create_function(|lua, value: Value| {
        let v: JsonValue = lua.from_value(value)?;
        serde_json::to_string(&v)
            .map_err(|e| mlua::Error::runtime(format!("nefor.json.encode: {e}")))
    })?;
    json.set("encode", encode)?;

    let decode = lua.create_function(|lua, s: String| {
        let v: JsonValue = serde_json::from_str(&s)
            .map_err(|e| mlua::Error::runtime(format!("nefor.json.decode: {e}")))?;
        lua.to_value(&v)
    })?;
    json.set("decode", decode)?;

    nefor_tbl.set("json", json)?;
    Ok(())
}

/// `nefor.fs.*` — the subset the shared `output-persistence` lib needs:
/// `data_root`, `mkdir_p`, `read_file`, `write_file`, `exists`. Errors return
/// as data (`{ ok, error }`), matching the engine's `install_fs` convention.
fn install_fs(lua: &Lua, nefor_tbl: &Table, data_root: String) -> Result<(), MagError> {
    let fs_tbl = lua.create_table()?;

    fs_tbl.set(
        "data_root",
        lua.create_function(move |_, _: ()| Ok(data_root.clone()))?,
    )?;
    fs_tbl.set(
        "mkdir_p",
        lua.create_function(|lua, path: String| ok_or_err(lua, std::fs::create_dir_all(&path)))?,
    )?;
    fs_tbl.set(
        "exists",
        lua.create_function(|_, path: String| Ok(Path::new(&path).exists()))?,
    )?;
    fs_tbl.set(
        "write_file",
        lua.create_function(|lua, (path, content): (String, String)| {
            ok_or_err(lua, std::fs::write(&path, content))
        })?,
    )?;
    fs_tbl.set(
        "read_file",
        lua.create_function(|lua, path: String| {
            let t = lua.create_table()?;
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    t.set("ok", true)?;
                    t.set("content", content)?;
                }
                Err(e) => {
                    t.set("ok", false)?;
                    t.set("error", e.to_string())?;
                }
            }
            Ok(t)
        })?,
    )?;

    nefor_tbl.set("fs", fs_tbl)?;
    Ok(())
}

fn ok_or_err(lua: &Lua, result: std::io::Result<()>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match result {
        Ok(()) => t.set("ok", true)?,
        Err(e) => {
            t.set("ok", false)?;
            t.set("error", e.to_string())?;
        }
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_kernel(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("kernel.lua");
        let mut f = std::fs::File::create(&path).expect("create kernel");
        f.write_all(body.as_bytes()).expect("write kernel");
        path
    }

    #[test]
    fn loads_a_table_returning_kernel() {
        let dir = std::env::temp_dir().join(format!("mag-kernel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write_kernel(&dir, "nefor.log(\"hi\")\nreturn { name = \"k\" }");
        let host = LuaHost::load_kernel(&path, None).expect("load");
        assert_eq!(host.kernel_name().as_deref(), Some("k"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_table_kernel() {
        let dir = std::env::temp_dir().join(format!("mag-kernel-nt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write_kernel(&dir, "return 42");
        let err = match LuaHost::load_kernel(&path, None) {
            Ok(_) => panic!("expected KernelNotTable error"),
            Err(e) => e,
        };
        assert!(matches!(err, MagError::KernelNotTable { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn surfaces_missing_kernel_file() {
        let err =
            match LuaHost::load_kernel(std::path::Path::new("/nonexistent/mag/kernel.lua"), None) {
                Ok(_) => panic!("expected KernelRead error"),
                Err(e) => e,
            };
        assert!(matches!(err, MagError::KernelRead { .. }));
    }

    #[test]
    fn json_and_now_and_emit_bindings_are_installed() {
        let dir = std::env::temp_dir().join(format!("mag-kernel-bind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // A kernel that exercises the new native surface and returns a table.
        let path = write_kernel(
            &dir,
            r#"
            assert(type(nefor.json) == "table", "json missing")
            assert(nefor.json.decode(nefor.json.encode({a=1})).a == 1, "json roundtrip")
            assert(type(nefor.now_ms) == "function" and nefor.now_ms() > 0, "now_ms")
            assert(type(nefor.fs.data_root) == "function", "fs.data_root")
            nefor.emit({ kind = "test.event", n = 7 })
            return { name = "bindings" }
            "#,
        );
        let host = LuaHost::load_kernel(&path, None).expect("load");
        let drained = host.drain_emits().expect("drain");
        assert_eq!(drained.len(), 1, "one queued emit");
        assert_eq!(
            drained[0].get("kind").and_then(JsonValue::as_str),
            Some("test.event")
        );
        // Draining again yields an empty queue.
        assert!(host.drain_emits().expect("drain2").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
