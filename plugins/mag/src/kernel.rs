//! Embedded Lua VM that hosts the MAG kernel.
//!
//! The kernel proper (actor inventory, lazy construction, routing, the fold
//! over graph modifications) is Lua-resident — see
//! `plugins/mag/docs/actor-model.md` and `docs/ir.md`. This module is the
//! Rust host: it creates the VM, installs the native surface the kernel
//! needs (log, json, fs, a millisecond clock, and a bus-emit queue), loads
//! the kernel entry file, and drives the kernel's execute seams
//! (`begin_run`, `start`, `bus_response`) from the plugin's dispatch loop.
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

/// The fold's verdict on one applied modification (`{ ok, error }` from the
/// kernel's `start` / `apply` seams).
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

/// `begin_run`'s verdict: whether the run context was created, plus the ids
/// of stale runs the kernel reaped at the session boundary (contexts left by
/// a previous session's never-terminated runs) — the host fails their
/// still-pending execute replies.
#[derive(Debug, Clone)]
pub struct BeginRunOutcome {
    pub ok: bool,
    pub error: Option<String>,
    pub reaped: Vec<String>,
}

/// Why a run context is torn down. Threaded through `end_run` onto every
/// `mag.actor_killed` the teardown emits — display semantics for consumers
/// (a completed run's sweep must not read as death), not mechanics: kill
/// handlers run and abort envelopes flush identically for every reason.
/// The kernel's fourth reason, `reaped` (session-boundary sweep), is minted
/// Lua-side inside `begin_run` and never passes through here.
#[derive(Debug, Clone, Copy)]
pub enum TeardownReason {
    RunComplete,
    RunFailed,
    Killed,
}

impl TeardownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::RunComplete => "run_complete",
            Self::RunFailed => "run_failed",
            Self::Killed => "killed",
        }
    }
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

#[derive(Debug, Clone)]
pub struct RuleTrigger {
    pub rule_id: String,
    pub function: String,
    pub source_actor: String,
    pub source_wire: String,
    pub emission_seq: u64,
    pub value: JsonValue,
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

    /// Serializable foreign declarations for MAG library validation. This is
    /// plain immutable data: no Lua constructor or other runtime capability
    /// crosses the evaluation boundary.
    pub fn registry_contracts(&self) -> Result<JsonValue, MagError> {
        let f: Option<Function> = self.kernel.get("registry_contracts")?;
        match f {
            Some(f) => {
                let value: Value = f.call(self.lua.array_metatable())?;
                Ok(self.lua.from_value(value)?)
            }
            None => Ok(JsonValue::Array(Vec::new())),
        }
    }

    /// Create the run's kernel context (inventory, router, modlog, observer)
    /// and emit `mag.run_started`. Run identity is injected, never ambient
    /// (docs/ir.md). The outcome carries the stale run ids the kernel reaped
    /// at the session boundary; a duplicate live `run_id` rejects.
    #[cfg(test)]
    pub fn begin_run(
        &self,
        run_id: &str,
        run_name: &str,
        session_id: Option<&str>,
    ) -> Result<BeginRunOutcome, MagError> {
        self.begin_run_with_principal(run_id, run_name, session_id, None)
    }

    pub fn begin_run_with_principal(
        &self,
        run_id: &str,
        run_name: &str,
        session_id: Option<&str>,
        principal: Option<&str>,
    ) -> Result<BeginRunOutcome, MagError> {
        let meta = self.lua.create_table()?;
        meta.set("run_id", run_id)?;
        meta.set("run_name", run_name)?;
        if let Some(s) = session_id {
            meta.set("session_id", s)?;
        }
        if let Some(p) = principal {
            meta.set("principal", p)?;
        }
        let f: Function = self.kernel.get("begin_run")?;
        let res: Table = f.call::<Table>(meta)?;
        Ok(BeginRunOutcome {
            ok: res.get::<Option<bool>>("ok")?.unwrap_or(false),
            error: res.get::<Option<String>>("error")?,
            reaped: res
                .get::<Option<Vec<String>>>("reaped")?
                .unwrap_or_default(),
        })
    }

    /// Apply a program's initial modification through the run's fold. Actors
    /// register at apply and construct lazily at their first satisfied input
    /// contract, so a synchronous program runs to completion inside this call;
    /// an async one (a provider round-trip pending) progresses via
    /// [`LuaHost::bus_response`].
    pub fn start(&self, run_id: &str, modification: &JsonValue) -> Result<ApplyOutcome, MagError> {
        let mod_val = self.lua.to_value(modification)?;
        let f: Function = self.kernel.get("start")?;
        let res: Table = f.call::<Table>((run_id, mod_val))?;
        apply_outcome(&res)
    }

    /// Apply one graph modification directly through a run's fold — the control
    /// plane's direct kernel op (docs/ir.md, "Kernel operations": modifications
    /// reach `actors`/`kills`/`messages` through the fold, and "the control
    /// plane reaches them directly"). The mid-run kill surface uses it: a
    /// `{ kills = [...] }` modification unroutes the target, hands it its final
    /// kill message (the factory's abort envelope reaches the bus), and drops
    /// its correlations. Returns the fold's verbatim `{ ok, error }`.
    pub fn apply(&self, run_id: &str, modification: &JsonValue) -> Result<ApplyOutcome, MagError> {
        let mod_val = self.lua.to_value(modification)?;
        let f: Function = self.kernel.get("apply")?;
        let res: Table = f.call::<Table>((run_id, mod_val))?;
        apply_outcome(&res)
    }

    pub fn take_rule_trigger(&self, run_id: &str) -> Result<Option<RuleTrigger>, MagError> {
        let f: Function = self.kernel.get("take_rule_trigger")?;
        let trigger: Option<Table> = f.call::<Option<Table>>(run_id)?;
        trigger
            .map(|trigger| {
                let source: Table = trigger.get("source")?;
                Ok(RuleTrigger {
                    rule_id: trigger.get("rule_id")?,
                    function: trigger.get("fn")?,
                    source_actor: source.get("actor")?,
                    source_wire: source.get("wire")?,
                    emission_seq: trigger.get("emission_seq")?,
                    value: self.lua.from_value(trigger.get::<Value>("value")?)?,
                })
            })
            .transpose()
    }

    pub fn fail_run(&self, run_id: &str, error: &str) -> Result<bool, MagError> {
        let f: Function = self.kernel.get("fail_run")?;
        Ok(f.call::<bool>((run_id, error))?)
    }

    pub fn steer_run(
        &self,
        run_id: &str,
        actor_id: &str,
        message: &JsonValue,
    ) -> Result<bool, MagError> {
        let f: Function = self.kernel.get("steer_run")?;
        let message = self.lua.to_value(message)?;
        Ok(f.call::<bool>((run_id, actor_id, message))?)
    }

    /// End a run: the kernel reaps the context's live actors through the fold
    /// (kill handlers run — abort/cancel envelopes land on the emit queue; the
    /// caller drains them) and drops the context. The reason stamps every
    /// `mag.actor_killed` the teardown emits, so consumers can tell a
    /// completed run's bookkeeping sweep from a real termination
    /// (docs/actor-model.md, Kill reasons). Returns whether a live context
    /// existed.
    pub fn end_run(&self, run_id: &str, reason: TeardownReason) -> Result<bool, MagError> {
        let f: Function = self.kernel.get("end_run")?;
        Ok(f.call::<bool>((run_id, reason.as_str()))?)
    }

    /// Interrupt a live run's in-flight work. Two shapes selected by
    /// `terminate`:
    ///
    /// * `terminate == false` (GRACEFUL — the lead's own turn): settle every
    ///   in-flight capability correlation as a failed reply ("interrupted by
    ///   user") and emit a `tool.cancel` for each. The failure routes through
    ///   the normal tool-failure path and the run STAYS ALIVE, winding down to a
    ///   real final answer (contrast [`LuaHost::end_run`]). The caller drains
    ///   the emit queue (the cancels + any re-fire the settle produced) and
    ///   settles the run only if it reached a terminal state.
    /// * `terminate == true` (TERMINATING — a dispatched sub-run): emit a
    ///   `tool.cancel` per open correlation so the real work dies, but deliver
    ///   NO reply — the run's llm never re-fires. The caller then ends the run
    ///   failed, so it settles `mag.run_result status:"failed"`.
    ///
    /// Returns `(count, terminated)`: correlations touched, and whether the
    /// terminating path ran. An unknown/ended run returns `(0, false)`.
    pub fn interrupt_run(
        &self,
        run_id: &str,
        failure: &str,
        terminate: bool,
    ) -> Result<(u64, bool), MagError> {
        let f: Function = self.kernel.get("interrupt_run")?;
        let res: Table = f.call::<Table>((run_id, failure, terminate))?;
        let count = res.get::<Option<u64>>("interrupted")?.unwrap_or(0);
        let terminated = res.get::<Option<bool>>("terminated")?.unwrap_or(false);
        Ok((count, terminated))
    }

    /// Deliver a correlated capability response (tool.result-shaped) back to
    /// the requesting actor, advancing any deferred activation it unblocks.
    /// Correlation ids are run-scoped, so the kernel dispatches to the owning
    /// run context and returns its run_id — the caller settles exactly that
    /// run. `None` means the id names no open correlation of ours.
    pub fn bus_response(
        &self,
        id: &str,
        result: Option<&JsonValue>,
        error: Option<&str>,
    ) -> Result<Option<String>, MagError> {
        let resp = self.lua.create_table()?;
        resp.set("id", id)?;
        if let Some(r) = result {
            resp.set("result", self.lua.to_value(r)?)?;
        }
        if let Some(e) = error {
            resp.set("error", e)?;
        }
        let f: Function = self.kernel.get("bus_response")?;
        Ok(f.call::<Option<String>>(resp)?)
    }

    /// Take a run's completion signal, if that run has finished.
    /// One-shot: clears the slot.
    pub fn take_run_complete(&self, run_id: &str) -> Result<Option<RunCompletion>, MagError> {
        let f: Option<Function> = self.kernel.get("take_run_complete")?;
        let f = match f {
            Some(f) => f,
            None => return Ok(None),
        };
        let rc: Option<Table> = f.call::<Option<Table>>(run_id)?;
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

    /// Take a run's unhandled-failure signal, if an actor failure escalated to
    /// a run failure (an unrouted failure tag — routing.lua apply_completion →
    /// `mag.run_failed`). One-shot: clears the slot. Returns the failure detail
    /// the run's terminal reply surfaces.
    pub fn take_run_failed(&self, run_id: &str) -> Result<Option<String>, MagError> {
        let f: Option<Function> = self.kernel.get("take_run_failed")?;
        let f = match f {
            Some(f) => f,
            None => return Ok(None),
        };
        let rf: Option<Table> = f.call::<Option<Table>>(run_id)?;
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

/// Read `{ ok, error }` off a fold-result table.
fn apply_outcome(res: &Table) -> Result<ApplyOutcome, MagError> {
    Ok(ApplyOutcome {
        ok: res.get::<Option<bool>>("ok")?.unwrap_or(false),
        error: res.get::<Option<String>>("error")?,
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
/// 3. the repo root's `lua/`, four levels above the kernel dir
///    (`.../plugins/mag/lua/mag-kernel` → root) — covers a bare `--kernel`
///    pointing into a checkout.
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
    // .../plugins/mag/lua/mag-kernel → the repo/config root is four levels up
    // (mag-kernel → lua → mag → plugins → root).
    if let Some(root) = dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
    {
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
    install_typed_json(lua, &nefor)?;
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

/// Rust-owned strict JSON parsing plus MAG schema validation. Keeping parsing
/// and validation in one host call avoids Lua's empty-table array/object
/// ambiguity and ensures runtime acceptance uses the compiler's descriptor.
fn install_typed_json(lua: &Lua, nefor_tbl: &Table) -> Result<(), MagError> {
    let typed_json = lua.create_table()?;
    let validate = lua.create_function(|lua, (schema, source): (Value, String)| {
        let encoded: JsonValue = lua.from_value(schema)?;
        let schema: nefor_mag::schema::TypeSchema = serde_json::from_value(encoded)
            .map_err(|error| mlua::Error::runtime(format!("invalid MAG type schema: {error}")))?;
        lua.to_value(&schema.validate_json(&source))
    })?;
    typed_json.set("validate", validate)?;
    nefor_tbl.set("typed_json", typed_json)?;
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

    // serde_json null crosses into Lua as mlua's dedicated NULL sentinel, not
    // nil. Expose an exact predicate so field-specific optional-value
    // boundaries can normalize it without treating arbitrary userdata as
    // absence or erasing meaningful nulls elsewhere in actor data.
    let is_null = lua.create_function(|_, value: Value| Ok(value.is_null()))?;
    json.set("is_null", is_null)?;

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

    fn shipped_host() -> LuaHost {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        LuaHost::load_kernel(
            &manifest.join("lua/mag-kernel/init.lua"),
            Some(&manifest.join("../../lua")),
        )
        .expect("load shipped kernel")
    }

    fn compile_mag_source(host: &LuaHost, name: &str, source: &str) -> JsonValue {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source_dir =
            std::env::temp_dir().join(format!("mag-kernel-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&source_dir);
        std::fs::create_dir_all(&source_dir).expect("create shell test workspace");
        std::fs::write(source_dir.join("main.mag"), source).expect("write shell test program");
        let contracts = host.registry_contracts().expect("runtime contracts");
        let loaded = nefor_mag::load_with_inputs_and_module_roots(
            &source_dir,
            "main.mag",
            serde_json::json!({"foreign_contracts": contracts}),
            &[manifest.join("../../starter/mag/lib")],
        )
        .expect("compile MAG test program");
        let artifact = serde_json::to_value(loaded.artifact).expect("serialize shell artifact");
        let modification =
            crate::artifact_modification(&artifact).expect("normalize shell artifact");
        let _ = std::fs::remove_dir_all(source_dir);
        modification
    }

    fn compile_mag_eval_expression(host: &LuaHost, name: &str, expression: &str) -> JsonValue {
        let source = format!(
            r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.shell")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      operation {expression}
      result (nefor.graph.output-for "result" operation)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start operation)
         (nefor.graph.edge operation result)]))))
"#
        );
        compile_mag_source(host, name, &source)
    }

    fn documented_shell_expression(needle: &str) -> String {
        let patterns = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../starter/mag/lib/patterns.md"),
        )
        .expect("read starter MAG patterns");
        patterns
            .split("```lisp\n")
            .skip(1)
            .filter_map(|rest| rest.split_once("\n```").map(|(source, _)| source))
            .find(|source| source.contains(needle))
            .unwrap_or_else(|| panic!("patterns.md has no Lisp fragment containing {needle}"))
            .to_owned()
    }

    fn start_shell_expression(
        host: &LuaHost,
        run_id: &str,
        expression: &str,
    ) -> Vec<Map<String, JsonValue>> {
        let modification = compile_mag_eval_expression(host, run_id, expression);
        let begun = host
            .begin_run(run_id, run_id, None)
            .expect("begin shell run");
        assert!(begun.ok, "begin failed: {:?}", begun.error);
        host.drain_emits().expect("drain begin event");
        let outcome = host.start(run_id, &modification).expect("start shell run");
        assert!(outcome.ok, "start failed: {:?}", outcome.error);
        host.drain_emits().expect("drain shell start")
    }

    fn tool_invoke<'a>(
        emits: &'a [Map<String, JsonValue>],
        command: &str,
    ) -> &'a Map<String, JsonValue> {
        emits
            .iter()
            .find(|event| {
                event.get("kind").and_then(JsonValue::as_str) == Some("tool.invoke")
                    && event["args"]["args"]["command"].as_str() == Some(command)
            })
            .unwrap_or_else(|| panic!("missing tool.invoke for {command}: {emits:#?}"))
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

    #[test]
    fn reads_registry_contract_snapshot_as_json() {
        let dir = std::env::temp_dir().join(format!("mag-kernel-contract-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write_kernel(
            &dir,
            r#"
            return {
              registry_contracts = function()
                return {{
                  identity = "nefor.factory.example",
                  implementation = "example",
                  params = { count = "int" },
                  type_scheme = {
                    variables = { "T" },
                    inputs = { value = "T" },
                    outputs = { "T" },
                  },
                  signals = {},
                }}
              end,
            }
            "#,
        );
        let host = LuaHost::load_kernel(&path, None).expect("load");
        let contracts = host.registry_contracts().expect("contracts");
        assert_eq!(contracts[0]["identity"], "nefor.factory.example");
        assert_eq!(contracts[0]["type_scheme"]["variables"][0], "T");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn capability_invocation_carries_authoritative_run_provenance() {
        let host = shipped_host();
        let run_id = "provenance-run";
        let modification = compile_mag_eval_expression(
            &host,
            run_id,
            r#"(nefor.shell.command "command" "printf provenance")"#,
        );
        let begun = host
            .begin_run_with_principal(run_id, "scout", Some("session-1"), Some("subagent"))
            .expect("begin provenance run");
        assert!(begun.ok, "begin failed: {:?}", begun.error);
        host.drain_emits().expect("drain begin event");
        let outcome = host.start(run_id, &modification).expect("start run");
        assert!(outcome.ok, "start failed: {:?}", outcome.error);
        let emits = host.drain_emits().expect("drain start");
        let invoke = tool_invoke(&emits, "printf provenance");
        let provenance = invoke["invocation"].as_object().expect("provenance");
        assert_eq!(provenance["session_id"], "session-1");
        assert_eq!(provenance["run_id"], run_id);
        assert_eq!(provenance["principal"], "subagent");
        assert_eq!(provenance["actor_id"], invoke["from"]);
        assert_eq!(provenance["capability_id"], invoke["id"]);
        assert!(provenance["run_scope"]
            .as_str()
            .is_some_and(|scope| scope.starts_with('r')));
    }

    #[test]
    fn canonical_shell_commands_cross_json_null_and_reach_capability_invocation() {
        let host = shipped_host();

        let command_emits = start_shell_expression(
            &host,
            "shell-command-null",
            r#"(nefor.shell.command "command" "printf command")"#,
        );
        let command = tool_invoke(&command_emits, "printf command");
        assert!(command["args"]["args"]
            .as_object()
            .expect("command capability args")
            .get("timeout_ms")
            .is_none());
        assert!(host
            .take_run_failed("shell-command-null")
            .expect("command run failure")
            .is_none());

        let pipe_program = r#"(require "nefor.artifact")
(require "nefor.graph")
(require "nefor.shell")
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      source (nefor.shell.command "source" "printf source")
      pipe (nefor.shell.pipe-command "pipe" "cat")
      result (nefor.graph.output-for "result" pipe)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start source)
         (nefor.graph.edge source pipe)
         (nefor.graph.edge pipe result)]))))"#;
        let pipe_modification = compile_mag_source(&host, "shell-pipe-null", pipe_program);
        let begun = host
            .begin_run("shell-pipe-null", "shell-pipe-null", None)
            .expect("begin pipe");
        assert!(begun.ok);
        host.drain_emits().expect("drain pipe begin");
        assert!(
            host.start("shell-pipe-null", &pipe_modification)
                .expect("start pipe")
                .ok
        );
        let pipe_emits = host.drain_emits().expect("drain source invocation");
        let source = tool_invoke(&pipe_emits, "printf source");
        let source_id = source["id"].as_str().expect("source correlation id");
        assert!(source["args"]["args"]
            .as_object()
            .expect("source capability args")
            .get("timeout_ms")
            .is_none());
        assert_eq!(
            host.bus_response(source_id, Some(&serde_json::json!("text\n[exit 0]")), None)
                .expect("source response"),
            Some("shell-pipe-null".into())
        );
        let pipe_emits = host.drain_emits().expect("drain pipe invocation");
        let pipe = tool_invoke(&pipe_emits, "cat");
        assert!(pipe["args"]["args"]
            .as_object()
            .expect("pipe capability args")
            .get("timeout_ms")
            .is_none());
        assert_eq!(pipe["args"]["args"]["stdin"], "text\n");
        assert!(host
            .take_run_failed("shell-pipe-null")
            .expect("pipe run failure")
            .is_none());
    }

    #[test]
    fn bounded_shell_timeout_is_preserved_and_invalid_bounds_still_fail() {
        let host = shipped_host();
        let documented = documented_shell_expression("nefor.shell.command-with-options");
        let emits = start_shell_expression(&host, "shell-bounded", &documented);
        let bounded = tool_invoke(&emits, "rg -n TODO src/");
        assert_eq!(bounded["args"]["args"]["timeout_ms"], 30000);
        assert!(host
            .take_run_failed("shell-bounded")
            .expect("bounded run failure")
            .is_none());

        for (run_id, bound) in [("shell-zero", "0"), ("shell-negative", "-1")] {
            let expression = format!(
                "(nefor.shell.command-with-options \"invalid\" \"true\" (as nefor.shell.BashOptions {{:timeout_ms {bound}}}))"
            );
            let emits = start_shell_expression(&host, run_id, &expression);
            assert!(emits
                .iter()
                .all(|event| event.get("kind").and_then(JsonValue::as_str) != Some("tool.invoke")));
            let failure = host
                .take_run_failed(run_id)
                .expect("invalid run failure")
                .expect("invalid timeout fails construction");
            assert!(
                failure.contains("positive number of milliseconds"),
                "{failure}"
            );
        }
    }

    #[test]
    fn structural_result_boundary_accepts_arbitrary_declared_wire() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest.join("lua/mag-kernel/init.lua");
        let lua_root = manifest.join("../../lua");
        let host = LuaHost::load_kernel(&path, Some(&lua_root)).expect("load shipped kernel");
        let begun = host
            .begin_run("custom-result", "custom-result", None)
            .expect("begin run");
        assert!(begun.ok, "begin failed: {:?}", begun.error);
        let modification = serde_json::json!({
            "actors": [{
                "id": "custom",
                "factory": "nefor.factory.stub",
                "params": {"greeting": "done"},
                "routes": {}
            }],
            "messages": [{"to": "custom", "content": {"kind": "stub.In"}}],
            "kills": [],
            "rules": [],
            "result": {"from": {
                "actor": "custom",
                "type": "example.CustomResult",
                "wire": "stub.Out"
            }}
        });
        let outcome = host.start("custom-result", &modification).expect("start");
        assert!(outcome.ok, "start failed: {:?}", outcome.error);
        let completion = host
            .take_run_complete("custom-result")
            .expect("read completion")
            .expect("custom result completed");
        assert_eq!(
            completion.result.as_ref().and_then(|v| v["kind"].as_str()),
            Some("stub.Out")
        );
        assert_eq!(
            completion
                .result
                .as_ref()
                .and_then(|v| v["greeting"].as_str()),
            Some("done")
        );
    }

    #[test]
    fn rules_bind_concrete_ports_and_require_canonical_values() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let host = LuaHost::load_kernel(
            &manifest.join("lua/mag-kernel/init.lua"),
            Some(&manifest.join("../../lua")),
        )
        .expect("load shipped kernel");

        host.begin_run("rule-payload", "rule-payload", None)
            .expect("begin");
        let graph = serde_json::json!({
            "actors": [
                {"id":"source", "factory":"nefor.factory.stub", "params":{}, "routes":{}},
                {"id":"result", "factory":"nefor.factory.stub", "params":{}, "routes":{}}
            ],
            "messages": [
                {"to":"source", "content":{"kind":"stub.In"}},
                {"to":"result", "content":{"kind":"stub.In"}}
            ],
            "kills": [],
            "rules": [{
                "id":"expand", "on":{"actor":"source", "wire":"stub.Out", "type":"String"},
                "fn":"expand"
            }],
            "result":{"from":{"actor":"result", "wire":"stub.Out", "type":"String"}}
        });
        let outcome = host.start("rule-payload", &graph).expect("start");
        assert!(outcome.ok, "start failed: {:?}", outcome.error);
        assert!(host
            .take_rule_trigger("rule-payload")
            .expect("trigger")
            .is_none());
        let failure = host
            .take_run_failed("rule-payload")
            .expect("failure")
            .expect("canonical payload failure");
        assert!(failure.contains("emitted no canonical value"), "{failure}");
        assert!(host
            .take_run_complete("rule-payload")
            .expect("completion")
            .is_none());

        host.begin_run("rule-ok", "rule-ok", None)
            .expect("begin canonical rule");
        let mut canonical = graph.clone();
        canonical["actors"][0]["params"]["value"] = serde_json::json!({"task":"one"});
        canonical["messages"] = serde_json::json!([
            {"to":"source", "content":{"kind":"stub.In", "n":1}},
            {"to":"source", "content":{"kind":"stub.In", "n":2}},
            {"to":"result", "content":{"kind":"stub.In"}}
        ]);
        let accepted = host.start("rule-ok", &canonical).expect("start canonical");
        assert!(accepted.ok, "start failed: {:?}", accepted.error);
        let trigger = host
            .take_rule_trigger("rule-ok")
            .expect("trigger")
            .expect("one canonical trigger");
        assert_eq!(trigger.rule_id, "expand");
        assert_eq!(trigger.source_actor, "source");
        assert_eq!(trigger.source_wire, "stub.Out");
        assert_eq!(trigger.emission_seq, 1);
        assert_eq!(trigger.value, serde_json::json!({"task":"one"}));

        host.begin_run("rule-isolated", "rule-isolated", None)
            .expect("begin isolated rule");
        let isolated = host
            .start("rule-isolated", &canonical)
            .expect("start isolated");
        assert!(isolated.ok);
        let isolated_trigger = host
            .take_rule_trigger("rule-isolated")
            .expect("isolated trigger")
            .expect("isolated queue");
        assert_eq!(isolated_trigger.emission_seq, 1);
        let second = host
            .take_rule_trigger("rule-ok")
            .expect("second trigger")
            .expect("FIFO second source emission");
        assert_eq!(second.emission_seq, 2);
        assert!(host
            .take_rule_trigger("rule-ok")
            .expect("quiescent first run")
            .is_none());

        host.begin_run("rule-fanout", "rule-fanout", None)
            .expect("begin rule fanout");
        let mut fanout = graph.clone();
        fanout["actors"][0]["params"]["value"] = serde_json::json!({"task":"fanout"});
        let mut second_rule = fanout["rules"][0].clone();
        second_rule["id"] = JsonValue::String("expand-again".into());
        fanout["rules"].as_array_mut().unwrap().push(second_rule);
        assert!(host.start("rule-fanout", &fanout).expect("fanout start").ok);
        assert_eq!(
            host.take_rule_trigger("rule-fanout")
                .unwrap()
                .unwrap()
                .rule_id,
            "expand"
        );
        assert_eq!(
            host.take_rule_trigger("rule-fanout")
                .unwrap()
                .unwrap()
                .rule_id,
            "expand-again"
        );

        host.begin_run("rule-result", "rule-result", None)
            .expect("begin result rule");
        let mut result_rule = graph;
        result_rule["rules"][0]["on"]["actor"] = JsonValue::String("result".into());
        let rejected = host
            .start("rule-result", &result_rule)
            .expect("start reject");
        assert!(!rejected.ok);
        assert!(rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("may not bind the result boundary")));

        host.begin_run("rule-duplicate", "rule-duplicate", None)
            .expect("begin duplicate rule");
        let mut duplicate = result_rule;
        duplicate["rules"][0]["on"]["actor"] = JsonValue::String("source".into());
        let copied = duplicate["rules"][0].clone();
        duplicate["rules"].as_array_mut().unwrap().push(copied);
        let duplicate_result = host
            .start("rule-duplicate", &duplicate)
            .expect("duplicate reject");
        assert!(!duplicate_result.ok);
        assert!(duplicate_result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("duplicate rule id")));

        for (run_id, messages) in [
            (
                "rule-invalid-valid",
                serde_json::json!([
                    {"to":"source", "content":{"kind":"stub.In"}},
                    {"to":"source", "content":{"kind":"stub.In", "value":{"ok":true}}},
                    {"to":"result", "content":{"kind":"stub.In"}}
                ]),
            ),
            (
                "rule-valid-invalid",
                serde_json::json!([
                    {"to":"source", "content":{"kind":"stub.In", "value":{"ok":true}}},
                    {"to":"source", "content":{"kind":"stub.In"}},
                    {"to":"result", "content":{"kind":"stub.In"}}
                ]),
            ),
        ] {
            host.begin_run(run_id, run_id, None)
                .expect("begin mixed emissions");
            let mut mixed = canonical.clone();
            mixed["actors"][0]["params"] = serde_json::json!({"canonical_from_message":true});
            mixed["messages"] = messages;
            let outcome = host.start(run_id, &mixed).expect("mixed start");
            assert!(outcome.ok);
            assert!(host
                .take_rule_trigger(run_id)
                .expect("disabled queue")
                .is_none());
            assert!(host.take_run_failed(run_id).expect("failed run").is_some());
            assert!(host
                .take_run_complete(run_id)
                .expect("no completion")
                .is_none());
        }

        host.begin_run("rule-stop-route", "rule-stop-route", None)
            .expect("begin stopped route");
        let stopped_route = serde_json::json!({
            "actors": [
                {"id":"source", "factory":"nefor.factory.stub", "params":{},
                 "routes":{"stub.Out":[{"actor":"consumer","wire":"stub.Out"}]}},
                {"id":"consumer", "factory":"nefor.factory.stub", "params":{}, "routes":{}}
            ],
            "messages":[{"to":"source", "content":{"kind":"stub.In"}}],
            "kills":[],
            "rules":[{"id":"expand", "on":{"actor":"source", "wire":"stub.Out", "type":"String"}, "fn":"expand"}],
            "result":{"from":{"actor":"consumer", "wire":"stub.Out", "type":"String"}}
        });
        assert!(
            host.start("rule-stop-route", &stopped_route)
                .expect("stopped route start")
                .ok
        );
        assert!(host
            .take_run_failed("rule-stop-route")
            .expect("route failure")
            .is_some());
        assert!(host
            .take_run_complete("rule-stop-route")
            .expect("consumer stayed unfired")
            .is_none());

        let immutable = host
            .apply(
                "rule-payload",
                &serde_json::json!({"actors":[], "messages":[], "kills":[], "rules":[{
                    "id":"late", "on":{"actor":"source", "wire":"stub.Out"}, "fn":"late"
                }]}),
            )
            .expect("delta apply");
        assert!(!immutable.ok);
        assert!(immutable
            .error
            .as_deref()
            .is_some_and(|error| error.contains("immutable initial subscriptions")));
    }
}
