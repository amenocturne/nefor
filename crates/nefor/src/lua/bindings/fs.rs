//! `nefor.fs` — synchronous filesystem primitives for Lua.
//!
//! Errors are returned as data (`{ ok = false, error = "..." }`) so the
//! caller decides what to do — they never bubble up as Lua exceptions.
//! Boolean-shaped queries (`exists`, `is_dir`, `is_symlink`) return
//! plain `bool` to keep the call-sites readable.
//!
//! Distinct from `plugins/nefor-tui/src/fs.rs` (`nefor.fs.list_dir`),
//! which is plugin-local to the TUI Lua VM. This module installs onto
//! the engine's Lua surface for `init.lua` consumers such as `pm`.

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

use mlua::{Lua, Table};

/// Install `nefor.fs.*` onto `nefor_tbl`.
pub fn install_fs(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let fs_tbl = lua.create_table()?;

    fs_tbl.set(
        "mkdir_p",
        lua.create_function(|lua, path: String| ok_or_err(lua, fs::create_dir_all(&path)))?,
    )?;

    fs_tbl.set(
        "exists",
        lua.create_function(|_, path: String| Ok(Path::new(&path).exists()))?,
    )?;

    fs_tbl.set(
        "is_dir",
        lua.create_function(|_, path: String| {
            // Resolves symlinks deliberately: callers asking "is_dir"
            // about a symlink want the target's shape.
            Ok(fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false))
        })?,
    )?;

    fs_tbl.set(
        "is_symlink",
        lua.create_function(|_, path: String| {
            Ok(fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false))
        })?,
    )?;

    fs_tbl.set(
        "read_link",
        lua.create_function(|_, path: String| match fs::read_link(&path) {
            Ok(target) => Ok(Some(target.to_string_lossy().into_owned())),
            Err(_) => Ok(None),
        })?,
    )?;

    fs_tbl.set(
        "symlink",
        lua.create_function(|lua, (target, link_path): (String, String)| {
            ok_or_err(lua, unix_fs::symlink(&target, &link_path))
        })?,
    )?;

    fs_tbl.set(
        "remove",
        lua.create_function(|lua, path: String| {
            // remove_file handles symlinks too. For real directories use
            // a separate caller (none of pm's call-sites need it).
            ok_or_err(lua, fs::remove_file(&path))
        })?,
    )?;

    fs_tbl.set(
        "read_file",
        lua.create_function(|lua, path: String| match fs::read_to_string(&path) {
            Ok(content) => {
                let t = lua.create_table()?;
                t.set("ok", true)?;
                t.set("content", content)?;
                Ok(t)
            }
            Err(e) => {
                let t = lua.create_table()?;
                t.set("ok", false)?;
                t.set("error", e.to_string())?;
                Ok(t)
            }
        })?,
    )?;

    fs_tbl.set(
        "write_file",
        lua.create_function(|lua, (path, content): (String, String)| {
            ok_or_err(lua, fs::write(&path, content))
        })?,
    )?;

    nefor_tbl.set("fs", fs_tbl)?;
    Ok(())
}

fn ok_or_err(lua: &Lua, result: std::io::Result<()>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match result {
        Ok(()) => {
            t.set("ok", true)?;
        }
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

    fn setup() -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        install_fs(&lua, &nefor).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        lua
    }

    #[test]
    fn mkdir_p_creates_nested_dirs_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c");
        let lua = setup();
        let ok1: bool = lua
            .load(format!(
                r#"return nefor.fs.mkdir_p("{}").ok"#,
                nested.display()
            ))
            .eval()
            .unwrap();
        assert!(ok1);
        assert!(nested.is_dir());
        let ok2: bool = lua
            .load(format!(
                r#"return nefor.fs.mkdir_p("{}").ok"#,
                nested.display()
            ))
            .eval()
            .unwrap();
        assert!(ok2, "mkdir_p must be idempotent");
    }

    #[test]
    fn exists_and_is_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("file.txt");
        std::fs::write(&f, "x").unwrap();
        let lua = setup();
        let (exists, is_dir): (bool, bool) = lua
            .load(format!(
                r#"return nefor.fs.exists("{0}"), nefor.fs.is_dir("{0}")"#,
                f.display()
            ))
            .eval()
            .unwrap();
        assert!(exists);
        assert!(!is_dir);
        let dir_is_dir: bool = lua
            .load(format!(r#"return nefor.fs.is_dir("{}")"#, tmp.path().display()))
            .eval()
            .unwrap();
        assert!(dir_is_dir);
    }

    #[test]
    fn symlink_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        let lua = setup();
        let ok: bool = lua
            .load(format!(
                r#"return nefor.fs.symlink("{}", "{}").ok"#,
                target.display(),
                link.display()
            ))
            .eval()
            .unwrap();
        assert!(ok);
        let is_link: bool = lua
            .load(format!(r#"return nefor.fs.is_symlink("{}")"#, link.display()))
            .eval()
            .unwrap();
        assert!(is_link);
        let read: Option<String> = lua
            .load(format!(r#"return nefor.fs.read_link("{}")"#, link.display()))
            .eval()
            .unwrap();
        assert_eq!(read.as_deref(), Some(target.to_string_lossy().as_ref()));
    }

    #[test]
    fn remove_deletes_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let lua = setup();
        let ok: bool = lua
            .load(format!(r#"return nefor.fs.remove("{}").ok"#, link.display()))
            .eval()
            .unwrap();
        assert!(ok);
        assert!(!link.exists() && !link.is_symlink());
        assert!(target.exists(), "remove must not touch symlink target");
    }

    #[test]
    fn read_write_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hello.txt");
        let lua = setup();
        let ok: bool = lua
            .load(format!(
                r#"return nefor.fs.write_file("{}", "hello world").ok"#,
                path.display()
            ))
            .eval()
            .unwrap();
        assert!(ok);
        let content: String = lua
            .load(format!(
                r#"return nefor.fs.read_file("{}").content"#,
                path.display()
            ))
            .eval()
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn read_missing_file_returns_error_as_data() {
        let lua = setup();
        let (ok, err): (bool, Option<String>) = lua
            .load(
                r#"
                local r = nefor.fs.read_file("/nope/definitely/not/here.xyz")
                return r.ok, r.error
                "#,
            )
            .eval()
            .unwrap();
        assert!(!ok);
        assert!(err.is_some());
    }
}
