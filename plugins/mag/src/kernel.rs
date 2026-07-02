//! Embedded Lua VM that hosts the MAG kernel.
//!
//! The kernel proper (actor inventory, ready barrier, mailboxes, routing,
//! the fold over graph modifications) is Lua-resident — see
//! `plugins/mag/docs/actor-model.md` and `docs/ir.md`. This module is the
//! thin Rust host: it creates the VM, installs the minimal native surface
//! the kernel needs, loads the kernel entry file, and holds the returned
//! table for the process lifetime.
//!
//! No evaluator behavior is wired yet — the `nefor-mag` crate is a
//! dependency but is not called from here in the skeleton.

use std::path::Path;

use mlua::{Lua, Table, Value};

use crate::error::MagError;

/// Owns the Lua VM and the kernel table it produced.
///
/// Kept alive for the whole session: the VM is the kernel's entire world
/// (per the actor model), so dropping it would tear the kernel down.
pub struct LuaHost {
    // Field order matters for drop: the kernel table references registry
    // state owned by `lua`, so `lua` must outlive it. Both are `mlua`
    // owned handles (no borrow), so this is safe regardless, but keeping
    // the VM last documents the intent.
    _kernel: Table,
    _lua: Lua,
}

impl LuaHost {
    /// Read, evaluate, and hold the kernel at `path`.
    ///
    /// The chunk is expected to return a table (the kernel). Anything
    /// else is a [`MagError::KernelNotTable`]. The kernel may call
    /// `nefor.log(msg)` during load; those messages route to the plugin's
    /// tracing subscriber (stderr), never stdout — stdout is the NCP wire.
    pub fn load_kernel(path: &Path) -> Result<Self, MagError> {
        let source = std::fs::read_to_string(path).map_err(|source| MagError::KernelRead {
            path: path.display().to_string(),
            source,
        })?;

        let lua = Lua::new();
        install_nefor(&lua)?;

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

        Ok(LuaHost {
            _kernel: kernel,
            _lua: lua,
        })
    }

    /// The kernel table's `name` field, if it exposed one.
    pub fn kernel_name(&self) -> Option<String> {
        kernel_name(&self._kernel)
    }
}

/// Read the `name` field off a kernel table, tolerating its absence or a
/// non-string value.
fn kernel_name(kernel: &Table) -> Option<String> {
    kernel.get::<Option<String>>("name").ok().flatten()
}

/// Install the minimal `nefor` global the kernel needs at load time.
///
/// For the skeleton that is just `nefor.log`, a bridge into the plugin's
/// tracing subscriber. The full surface (bus send/deliver, json, spawn)
/// lands with the kernel behavior.
fn install_nefor(lua: &Lua) -> Result<(), MagError> {
    let nefor = lua.create_table()?;
    let log = lua.create_function(|_, msg: String| {
        tracing::info!(target: "mag::kernel", "{msg}");
        Ok(())
    })?;
    nefor.set("log", log)?;
    lua.globals().set("nefor", nefor)?;
    Ok(())
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
        let host = LuaHost::load_kernel(&path).expect("load");
        assert_eq!(host.kernel_name().as_deref(), Some("k"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_table_kernel() {
        let dir = std::env::temp_dir().join(format!("mag-kernel-nt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write_kernel(&dir, "return 42");
        let err = match LuaHost::load_kernel(&path) {
            Ok(_) => panic!("expected KernelNotTable error"),
            Err(e) => e,
        };
        assert!(matches!(err, MagError::KernelNotTable { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn surfaces_missing_kernel_file() {
        let err = match LuaHost::load_kernel(std::path::Path::new("/nonexistent/mag/kernel.lua")) {
            Ok(_) => panic!("expected KernelRead error"),
            Err(e) => e,
        };
        assert!(matches!(err, MagError::KernelRead { .. }));
    }
}
