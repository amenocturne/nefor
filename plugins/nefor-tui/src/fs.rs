//! `nefor.fs` — Rust-backed filesystem helpers bridged into Lua.
//!
//! Surface:
//! - `nefor.fs.list_dir(path) -> { { name=string, is_dir=bool }, ... } | nil, err`
//!
//! Why: chat.lua's @-path autocomplete (issue #62 / `ls_entries`) was
//! shelling out via `io.popen("ls -1Ap …")` for every uncached directory.
//! Fine in interactive use; under parallel TUI tests on macOS the
//! per-fork cost compounds (each test runs a popen in CWD; concurrent
//! popens contend on the fork() path and inflate wall-time from ~90ms
//! to >60s in the at-path-autocomplete suite). The shell-out also
//! produced `shell-init: cwd not found` warnings when tests inherited
//! a since-deleted tempdir cwd from a sibling.
//!
//! Replacing the popen with a direct readdir keeps the same data shape
//! the Lua side consumes (name + is_dir per entry, all entries except
//! `.`/`..`) without spawning a subprocess.
//!
//! ## Return shape
//!
//! Success → a Lua array of records:
//!
//! ```lua
//! local entries, err = nefor.fs.list_dir("src")
//! -- entries = { { name = "main.rs", is_dir = false }, { name = "lib", is_dir = true }, ... }
//! ```
//!
//! Failure (path missing, not a directory, permission denied) → returns
//! `(nil, error_string)`. The Lua side translates nil to an empty list
//! so a half-typed directory name silently produces "no matches" rather
//! than a Lua error.
//!
//! ## Ordering
//!
//! Entries are returned in filesystem order. Caller is responsible for
//! sorting — keeps the binding policy-free so other consumers can pick
//! their own sort key.
//!
//! ## Hidden files
//!
//! Included. Matches BSD `ls -1A` semantics (everything except `.` and
//! `..`). The Lua side filters dotfiles for the @-completion use case.
//!
//! ## Non-UTF-8 file names
//!
//! Skipped with a `tracing::warn!`. Rare on macOS; the Lua side cannot
//! represent non-UTF-8 strings without lossy conversion, and losing one
//! file name from a completion popup is preferable to either failing
//! the whole listing or surfacing a corrupted string.

use mlua::{Lua, Table};

/// Install `nefor.fs.list_dir` onto `nefor_tbl`.
pub fn install_fs(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let fs = lua.create_table()?;

    let list_dir_fn = lua.create_function(|lua, path: String| {
        let read_dir = match std::fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => {
                // (nil, err) — matches the convention Lua callers expect
                // for "expected failure" paths (vs. a thrown error).
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
            // Prefer file_type() over a per-entry stat: on macOS readdir
            // returns d_type so this is a free field-read most of the
            // time; only symlinks-to-dirs trip the fallback stat.
            let is_dir = match entry.file_type() {
                Ok(ft) => ft.is_dir(),
                Err(_) => false,
            };
            let rec = lua.create_table()?;
            rec.set("name", name)?;
            rec.set("is_dir", is_dir)?;
            i += 1;
            out.set(i, rec)?;
        }
        Ok((mlua::Value::Table(out), mlua::Value::Nil))
    })?;
    fs.set("list_dir", list_dir_fn)?;

    nefor_tbl.set("fs", fs)?;
    Ok(())
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
    fn list_dir_returns_entries_with_dir_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("file.txt"), "x").expect("write");
        std::fs::write(dir.path().join(".hidden"), "y").expect("write hidden");

        let lua = setup();
        lua.globals()
            .set("test_path", dir.path().to_str().unwrap())
            .unwrap();
        let ok: bool = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir(test_path)
                if err ~= nil then return false end
                if entries == nil then return false end
                local by_name = {}
                for _, e in ipairs(entries) do by_name[e.name] = e end
                return by_name["sub"] ~= nil and by_name["sub"].is_dir == true
                   and by_name["file.txt"] ~= nil and by_name["file.txt"].is_dir == false
                   and by_name[".hidden"] ~= nil and by_name[".hidden"].is_dir == false
                "#,
            )
            .eval()
            .unwrap();
        assert!(
            ok,
            "list_dir should report sub/ as dir, file.txt/.hidden as files"
        );
    }

    #[test]
    fn list_dir_missing_path_returns_nil_and_error() {
        let lua = setup();
        let (entries_is_nil, err_is_string): (bool, bool) = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir("/this/path/definitely/does/not/exist")
                return entries == nil, type(err) == "string"
                "#,
            )
            .eval()
            .unwrap();
        assert!(entries_is_nil, "missing path should return nil entries");
        assert!(err_is_string, "missing path should return a string error");
    }

    #[test]
    fn list_dir_on_file_returns_nil_and_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("regular.txt");
        std::fs::write(&file, "x").expect("write");

        let lua = setup();
        lua.globals()
            .set("file_path", file.to_str().unwrap())
            .unwrap();
        let entries_is_nil: bool = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir(file_path)
                return entries == nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(
            entries_is_nil,
            "calling list_dir on a regular file should return nil"
        );
    }

    #[test]
    fn list_dir_empty_returns_empty_table() {
        let dir = tempfile::tempdir().expect("tempdir");

        let lua = setup();
        lua.globals()
            .set("test_path", dir.path().to_str().unwrap())
            .unwrap();
        let (is_table, len): (bool, i64) = lua
            .load(
                r#"
                local entries, err = nefor.fs.list_dir(test_path)
                return type(entries) == "table", #entries
                "#,
            )
            .eval()
            .unwrap();
        assert!(is_table, "empty dir listing should be a (empty) table");
        assert_eq!(len, 0, "empty dir should produce zero entries");
    }

    #[test]
    fn list_dir_does_not_include_dot_or_dotdot() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a"), "x").expect("write");

        let lua = setup();
        lua.globals()
            .set("test_path", dir.path().to_str().unwrap())
            .unwrap();
        let has_dot_entries: bool = lua
            .load(
                r#"
                local entries = nefor.fs.list_dir(test_path)
                for _, e in ipairs(entries) do
                  if e.name == "." or e.name == ".." then return true end
                end
                return false
                "#,
            )
            .eval()
            .unwrap();
        assert!(
            !has_dot_entries,
            "list_dir must not surface . or .. (matches BSD ls -A)"
        );
    }
}
