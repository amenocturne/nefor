//! `nefor.fs` — synchronous filesystem primitives for Lua.
//!
//! Errors are returned as data (`{ ok = false, error = "..." }`) so the
//! caller decides what to do — they never bubble up as Lua exceptions.
//! Boolean-shaped queries (`exists`, `is_dir`, `is_symlink`) return
//! plain `bool` to keep the call-sites readable.
//!
//! `nefor.fs.list_dir(path) -> { { name=string, is_dir=bool }, ... } | nil, err`
//! enumerates a directory non-recursively, skipping `.` / `..`. `is_dir`
//! follows symlinks (matches the convention `nefor.fs.is_dir` already
//! uses on this surface). Returns `(nil, err)` for missing path, not-a-
//! directory, or permission-denied so callers can branch on shape.
//!
//! The TUI plugin's `plugins/nefor-tui/src/fs.rs` exposes a similarly-
//! named binding scoped to its own Lua VM. Engine-side actors
//! (read-only-tools, etc.) need their own copy here.

use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use mlua::{Lua, Table};

use crate::paths::DataDir;

/// Install `nefor.fs.*` onto `nefor_tbl`.
///
/// `data_dir` is the engine's canonical resolved data directory (CLI flag,
/// then `NEFOR_DATA_DIR`, then `XDG_DATA_HOME/nefor`). It's captured into
/// `nefor.fs.data_root()` so Lua reads the same value the engine resolved
/// at startup — without re-evaluating env vars on the Lua side (which
/// historically drifted: Lua-side helpers invented a `NEFOR_DATA_HOME`
/// the Rust resolver doesn't know about).
pub fn install_fs(lua: &Lua, nefor_tbl: &Table, data_dir: DataDir) -> mlua::Result<()> {
    let fs_tbl = lua.create_table()?;

    let data_root: PathBuf = data_dir.as_path().to_path_buf();
    let data_root_string = data_root.to_string_lossy().into_owned();
    fs_tbl.set(
        "data_root",
        lua.create_function(move |_, _: ()| Ok(data_root_string.clone()))?,
    )?;

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

    fs_tbl.set(
        "list_dir",
        lua.create_function(|lua, path: String| {
            let read_dir = match fs::read_dir(&path) {
                Ok(rd) => rd,
                Err(e) => {
                    let err = lua.create_string(format!("nefor.fs.list_dir({path:?}): {e}"))?;
                    return Ok((mlua::Value::Nil, mlua::Value::String(err)));
                }
            };
            let out = lua.create_table()?;
            let mut i = 0i64;
            for entry in read_dir {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(target: "nefor::fs", error = %e, "skip readdir entry");
                        continue;
                    }
                };
                let name_os = entry.file_name();
                let name = match name_os.to_str() {
                    Some(s) => s.to_owned(),
                    None => {
                        tracing::warn!(
                            target: "nefor::fs",
                            name = ?name_os,
                            "skip non-UTF-8 file name"
                        );
                        continue;
                    }
                };
                // fs::metadata follows symlinks so a symlink-to-dir
                // reports is_dir=true, matching the engine's existing
                // nefor.fs.is_dir convention.
                let is_dir = fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false);
                let rec = lua.create_table()?;
                rec.set("name", name)?;
                rec.set("is_dir", is_dir)?;
                i += 1;
                out.set(i, rec)?;
            }
            Ok((mlua::Value::Table(out), mlua::Value::Nil))
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
        setup_with_data_dir(PathBuf::from("/var/empty/nefor-test-data-dir"))
    }

    fn setup_with_data_dir(data_dir: PathBuf) -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        install_fs(&lua, &nefor, DataDir(data_dir)).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        lua
    }

    #[test]
    fn data_root_returns_engine_resolved_path() {
        let lua = setup_with_data_dir(PathBuf::from("/some/explicit/data"));
        let got: String = lua
            .load("return nefor.fs.data_root()")
            .eval()
            .unwrap();
        assert_eq!(got, "/some/explicit/data");
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

    #[test]
    fn list_dir_returns_entries_with_dir_flag() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();
        std::fs::write(tmp.path().join(".hidden"), "y").unwrap();
        let lua = setup();
        lua.globals()
            .set("test_path", tmp.path().to_str().unwrap())
            .unwrap();
        let ok: bool = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir(test_path)
                if err ~= nil then return false end
                local by_name = {}
                for _, e in ipairs(entries) do by_name[e.name] = e end
                return by_name["sub"]      and by_name["sub"].is_dir == true
                   and by_name["file.txt"] and by_name["file.txt"].is_dir == false
                   and by_name[".hidden"]  and by_name[".hidden"].is_dir == false
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn list_dir_missing_path_returns_nil_and_error() {
        let lua = setup();
        let (entries_is_nil, err_is_string): (bool, bool) = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir("/nope/not/a/path/here")
                return entries == nil, type(err) == "string"
                "#,
            )
            .eval()
            .unwrap();
        assert!(entries_is_nil);
        assert!(err_is_string);
    }

    #[test]
    fn list_dir_skips_dot_and_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "x").unwrap();
        let lua = setup();
        lua.globals()
            .set("test_path", tmp.path().to_str().unwrap())
            .unwrap();
        let has_dot: bool = lua
            .load(
                r#"
                for _, e in ipairs(nefor.fs.list_dir(test_path)) do
                  if e.name == "." or e.name == ".." then return true end
                end
                return false
                "#,
            )
            .eval()
            .unwrap();
        assert!(!has_dot);
    }

    #[test]
    fn list_dir_follows_symlink_to_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real-dir")).unwrap();
        unix_fs::symlink(tmp.path().join("real-dir"), tmp.path().join("link-to-dir")).unwrap();
        let lua = setup();
        lua.globals()
            .set("test_path", tmp.path().to_str().unwrap())
            .unwrap();
        let link_reports_dir: bool = lua
            .load(
                r#"
                for _, e in ipairs(nefor.fs.list_dir(test_path)) do
                  if e.name == "link-to-dir" then return e.is_dir end
                end
                return false
                "#,
            )
            .eval()
            .unwrap();
        assert!(link_reports_dir, "symlink-to-dir should report is_dir=true");
    }
}
