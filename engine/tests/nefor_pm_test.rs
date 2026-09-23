//! Tests for `lua/nefor-pm/init.lua`.
//!
//! Post-sync refactor every test runs on a plain `Lua::new()` /
//! `eval()` harness — pm.install drives `nefor.process.run` and
//! `nefor.fs.*` synchronously, so the tokio multi-thread runtime
//! the previous version needed is gone.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Resolve `<repo-root>/lua/`. CARGO_MANIFEST_DIR points at engine/,
/// so we walk up one level.
fn lua_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("repo root is one level above engine")
        .join("lua")
}

/// Install the bare minimum `nefor.*` surface the pm module needs to load
/// without errors:
///   - `nefor.json` (encode/decode)
///   - `nefor.process.run` (used at install time)
///   - `nefor.fs.*` (used at install time and for lockfile IO)
fn install_nefor(lua: &Lua) -> mlua::Result<()> {
    let nefor = lua.create_table()?;
    nefor::lua::bindings::install_json(lua, &nefor)?;
    let (runtime_callback_tx, _runtime_callback_rx) = tokio::sync::mpsc::unbounded_channel();
    let runtime_processes = Arc::new(Mutex::new(Vec::new()));
    nefor::lua::bindings::install_process(lua, &nefor, runtime_callback_tx, runtime_processes)?;
    // pm's data_root() now delegates to `nefor.fs.data_root()` — capture
    // the resolved value from NEFOR_DATA_DIR (the DataDirGuard sets it
    // before constructing this VM). When unset, the resolver falls back
    // to XDG/HOME, but pm tests always pin the env so the unset branch
    // is moot here.
    let data_dir_path = std::env::var("NEFOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/empty/nefor-pm-test-data"));
    nefor::lua::bindings::install_fs(lua, &nefor, nefor::paths::DataDir::new(data_dir_path))?;
    lua.globals().set("nefor", nefor)?;
    Ok(())
}

fn set_pm_on_path(lua: &Lua) -> mlua::Result<()> {
    let dir = lua_dir();
    // `require("nefor-pm")` looks under <dir>/nefor-pm/init.lua via the
    // `?/init.lua` pattern. The `?.lua` pattern is in for sibling modules.
    let script = format!(
        r#"package.path = "{0}/?.lua;{0}/?/init.lua;" .. package.path"#,
        dir.display()
    );
    lua.load(&script).exec()
}

/// Lua + nefor table + pm on package.path. Caller still has to `require("nefor-pm")`.
fn lua_with_pm() -> Lua {
    let lua = Lua::new();
    install_nefor(&lua).expect("install nefor surface");
    set_pm_on_path(&lua).expect("pm on path");
    lua
}

/// Process-global lock to serialise tests that mutate NEFOR_DATA_DIR.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Scoped data-dir override. Sets NEFOR_DATA_DIR + unsets XDG_DATA_HOME
/// so the resolver lands on the tempdir deterministically. nefor-pm's
/// data_root() now delegates to `nefor.fs.data_root()` which is also
/// captured from this same env var at install time (see `install_nefor`).
struct DataDirGuard {
    prev_data_dir: Option<String>,
    prev_xdg: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl DataDirGuard {
    fn new(path: &std::path::Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_data_dir = std::env::var("NEFOR_DATA_DIR").ok();
        let prev_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("NEFOR_DATA_DIR", path);
        std::env::remove_var("XDG_DATA_HOME");
        Self {
            prev_data_dir,
            prev_xdg,
            _lock: lock,
        }
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        match self.prev_data_dir.as_deref() {
            Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
            None => std::env::remove_var("NEFOR_DATA_DIR"),
        }
        match self.prev_xdg.as_deref() {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

// ---------------------------------------------------------------------------
// Sync tests — spec parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_spec_accepts_shorthand_and_name() {
    let lua = lua_with_pm();
    let ok: bool = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local s = pm._internals.parse_spec({
              "amenocturne/nefor",
              name = "nefor-libs",
              tag = "v0.1.5",
              path = "lua-libs/",
            }, 1)
            return s.name == "nefor-libs"
              and s.url == "https://github.com/amenocturne/nefor.git"
              and s.ref == "v0.1.5"
              and s.ref_kind == "tag"
              and s.path == "lua-libs/"
            "#,
        )
        .eval()
        .expect("eval");
    assert!(ok, "shorthand parse should produce normalized spec");
}

#[test]
fn parse_spec_defaults_to_main_branch() {
    let lua = lua_with_pm();
    let ok: bool = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local s = pm._internals.parse_spec({
              "owner/repo", name = "x",
            }, 1)
            return s.ref == "main" and s.ref_kind == "branch"
            "#,
        )
        .eval()
        .expect("eval");
    assert!(ok);
}

#[test]
fn parse_spec_defaults_to_version_tag_when_set() {
    let lua = lua_with_pm();
    lua.load(r#"nefor.version = "0.1.9""#)
        .exec()
        .expect("set version");
    let (ref_val, kind): (String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local s = pm._internals.parse_spec({
              "owner/repo", name = "x",
            }, 1)
            return s.ref, s.ref_kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(ref_val, "v0.1.9", "exact semver → tag ref");
    assert_eq!(kind, "tag", "exact semver → tag kind");
}

#[test]
fn parse_spec_defaults_to_main_for_nightly_version() {
    let lua = lua_with_pm();
    lua.load(r#"nefor.version = "0.1.9-12-gabcdef""#)
        .exec()
        .expect("set version");
    let (ref_val, kind): (String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local s = pm._internals.parse_spec({
              "owner/repo", name = "x",
            }, 1)
            return s.ref, s.ref_kind
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(ref_val, "main", "nightly version → main branch");
    assert_eq!(kind, "branch", "nightly version → branch kind");
}

#[test]
fn engine_ref_returns_tag_for_exact_semver() {
    let lua = lua_with_pm();
    lua.load(r#"nefor.version = "1.2.3""#)
        .exec()
        .expect("set version");
    let (ref_val, kind): (String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            return pm.engine_ref()
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(ref_val, "v1.2.3");
    assert_eq!(kind, "tag");
}

#[test]
fn engine_ref_returns_main_when_no_version() {
    let lua = lua_with_pm();
    let (ref_val, kind): (String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            return pm.engine_ref()
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(ref_val, "main");
    assert_eq!(kind, "branch");
}

#[test]
fn parse_spec_rejects_missing_name() {
    let lua = lua_with_pm();
    let err = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            pm._internals.parse_spec({ "owner/repo" }, 3)
            "#,
        )
        .exec()
        .expect_err("missing name must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("spec #3 missing required `name` field"),
        "got: {msg}"
    );
    assert!(msg.contains("non-empty string"), "got: {msg}");
}

#[test]
fn parse_spec_rejects_empty_string_name() {
    let lua = lua_with_pm();
    let err = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            pm._internals.parse_spec({ "owner/repo", name = "" }, 2)
            "#,
        )
        .exec()
        .expect_err("empty name must fail");
    assert!(err.to_string().contains("non-empty string"), "got: {err}");
}

#[test]
fn parse_spec_rejects_conflicting_tag_and_commit() {
    let lua = lua_with_pm();
    let err = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            pm._internals.parse_spec({
              "o/r", name = "x", tag = "v1", commit = "deadbeef",
            }, 1)
            "#,
        )
        .exec()
        .expect_err("tag+commit must fail");
    assert!(err.to_string().contains("at most one"));
}

#[test]
fn parse_spec_rejects_no_source() {
    let lua = lua_with_pm();
    let err = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            pm._internals.parse_spec({ name = "x" }, 1)
            "#,
        )
        .exec()
        .expect_err("missing source must fail");
    assert!(err.to_string().contains("clonable source"));
}

#[test]
fn parse_spec_dev_override_skips_source_requirement() {
    let lua = lua_with_pm();
    let ok: bool = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local s = pm._internals.parse_spec({
              name = "x", dir = "/tmp/x",
            }, 1)
            return s.dir == "/tmp/x"
            "#,
        )
        .eval()
        .expect("eval");
    assert!(ok);
}

#[test]
fn parse_spec_rejects_url_and_shorthand_together() {
    let lua = lua_with_pm();
    let err = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            pm._internals.parse_spec({
              "o/r", name = "x", url = "https://example.com/r.git",
            }, 1)
            "#,
        )
        .exec()
        .expect_err("shorthand+url must fail");
    assert!(err.to_string().contains("shorthand"));
}

// ---------------------------------------------------------------------------
// Sync tests — lockfile round-trip
// ---------------------------------------------------------------------------

#[test]
fn lockfile_roundtrip_sorted_keys() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());

    let lua = lua_with_pm();
    lua.load(
        r#"
        pm = require("nefor-pm")
        pm._internals.write_lockfile({
          zeta  = { ref = "v1", commit = "aaa", build_hash = nil },
          alpha = { ref = "v2", commit = "bbb", build_hash = "h2" },
          mid   = { ref = "v3", commit = "ccc" },
        })
        "#,
    )
    .exec()
    .expect("write");

    let body = std::fs::read_to_string(tempdir.path().join("plugins").join("nefor-pm.lock.json"))
        .expect("read lockfile");
    // Keys must be sorted alphabetically: alpha, mid, zeta.
    let alpha_pos = body.find("\"alpha\"").expect("alpha present");
    let mid_pos = body.find("\"mid\"").expect("mid present");
    let zeta_pos = body.find("\"zeta\"").expect("zeta present");
    assert!(
        alpha_pos < mid_pos && mid_pos < zeta_pos,
        "keys must be sorted; got {body}"
    );

    let ok: bool = lua
        .load(
            r#"
            local lock = pm._internals.read_lockfile()
            return lock.alpha.commit == "bbb"
              and lock.mid.commit == "ccc"
              and lock.zeta.ref == "v1"
            "#,
        )
        .eval()
        .expect("read");
    assert!(ok);
}

#[test]
fn lockfile_read_missing_returns_empty() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());

    let lua = lua_with_pm();
    let n: i64 = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local lock = pm._internals.read_lockfile()
            local count = 0
            for _ in pairs(lock) do count = count + 1 end
            return count
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
#[test]
fn immutable_registration_loads_without_mutating_data_root() {
    let source = tempfile::tempdir().expect("source");
    let data = tempfile::tempdir().expect("data");
    let _g = DataDirGuard::new(data.path());
    let plug_dir = source.path().join("immutable-lib");
    std::fs::create_dir_all(plug_dir.join("sub")).expect("mkdir");
    std::fs::write(plug_dir.join("init.lua"), "return { value = 'root' }\n").expect("root");
    std::fs::write(plug_dir.join("sub/init.lua"), "return { value = 'sub' }\n").expect("sub");

    let lua = lua_with_pm();
    let values: (String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.register({{ {{ name = "immutable-lib", dir = "{}" }} }})
            return pm.load("immutable-lib").value, pm.load("immutable-lib.sub").value
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("registered modules load");
    assert_eq!(values, ("root".into(), "sub".into()));
    assert!(
        !data.path().join("plugins").exists(),
        "read-only registration must not create pm state"
    );
}

#[test]
fn root_resolves_registered_consumer_neutral_package() {
    let source = tempfile::tempdir().expect("source");
    let data = tempfile::tempdir().expect("data");
    let _g = DataDirGuard::new(data.path());
    std::fs::create_dir_all(source.path().join("lib")).expect("package lib");

    let lua = lua_with_pm();
    let root: String = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.register({{ {{ name = "nefor-mag", dir = "{}" }} }})
            return pm.root("nefor-mag")
            "#,
            source.path().display()
        ))
        .eval()
        .expect("registered package root");
    assert_eq!(root, source.path().display().to_string());
    assert!(!data.path().join("plugins").exists());
}

#[test]
fn root_resolves_dir_override_and_existing_managed_package() {
    let data = tempfile::tempdir().expect("data");
    let source = tempfile::tempdir().expect("source");
    let _g = DataDirGuard::new(data.path());
    std::fs::create_dir_all(source.path().join("book")).expect("source package");

    let lua = lua_with_pm();
    let override_root: String = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "custom-mag", dir = "{}" }} }})
            return pm.root("custom-mag")
            "#,
            source.path().display()
        ))
        .eval()
        .expect("dir override root");
    assert_eq!(override_root, source.path().display().to_string());

    let materialized = data.path().join("plugins").join("existing-data");
    std::fs::create_dir_all(materialized.join("lib")).expect("managed data package");
    let later_process = lua_with_pm();
    let existing_root: String = later_process
        .load(r#"return require("nefor-pm").root("existing-data")"#)
        .eval()
        .expect("existing managed root");
    assert_eq!(existing_root, materialized.display().to_string());
}

#[test]
fn immutable_registration_accepts_namespace_without_root_module() {
    let source = tempfile::tempdir().expect("source");
    let data = tempfile::tempdir().expect("data");
    let _g = DataDirGuard::new(data.path());
    let namespace = source.path().join("libs");
    std::fs::create_dir_all(namespace.join("child")).expect("mkdir");
    std::fs::write(
        namespace.join("child/init.lua"),
        "return { value = 'child' }\n",
    )
    .expect("child module");

    let lua = lua_with_pm();
    let value: String = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.register({{ {{ name = "libs", dir = "{}" }} }})
            return pm.load("libs.child").value
            "#,
            namespace.display()
        ))
        .eval()
        .expect("registered namespace loads children");
    assert_eq!(value, "child");
}

#[test]
fn immutable_registration_rejects_rebinding() {
    let first = tempfile::tempdir().expect("first");
    let second = tempfile::tempdir().expect("second");
    let data = tempfile::tempdir().expect("data");
    let _g = DataDirGuard::new(data.path());
    for dir in [first.path(), second.path()] {
        std::fs::write(dir.join("init.lua"), "return {}\n").expect("module");
    }
    let lua = lua_with_pm();
    let err = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.register({{ {{ name = "fixed", dir = "{}" }} }})
            pm.register({{ {{ name = "fixed", dir = "{}" }} }})
            "#,
            first.path().display(),
            second.path().display()
        ))
        .exec()
        .expect_err("rebind must fail");
    assert!(err.to_string().contains("different source"), "{err}");
}

// Sync tests — pm.load via dir override + pm.bin
// ---------------------------------------------------------------------------

#[test]
fn dir_override_install_makes_require_work() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());
    let plug_dir = tempdir.path().join("my-plugin");
    std::fs::create_dir_all(&plug_dir).expect("mkdir");
    std::fs::write(
        plug_dir.join("init.lua"),
        "return { hello = function() return 'world' end }\n",
    )
    .expect("write module");

    let lua = lua_with_pm();
    let ok: bool = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "my-plugin", dir = "{}" }} }})
            local mod = pm.load("my-plugin")
            return mod.hello() == "world"
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("eval");
    assert!(ok);

    // The install must have placed a symlink at <plugins_root>/<name>
    // pointing at the dev dir.
    let link = tempdir.path().join("plugins").join("my-plugin");
    let target = std::fs::read_link(&link).expect("read_link");
    assert_eq!(target, plug_dir, "symlink target");
}

#[test]
fn dir_override_basename_can_differ_from_name() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("data");
    let _g = DataDirGuard::new(data.path());
    // dir basename = "lua", name = "nefor-tui". Pre-fix this combination
    // was rejected at parse time; post-fix it resolves via the symlink.
    let plug_dir = tempdir.path().join("repo/plugins/nefor-tui/lua");
    std::fs::create_dir_all(&plug_dir).expect("mkdir");
    std::fs::write(plug_dir.join("init.lua"), "return { tag = 'nefor-tui' }\n").expect("write");

    let lua = lua_with_pm();
    let tag: String = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "nefor-tui", dir = "{}" }} }})
            return pm.load("nefor-tui").tag
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("eval");
    assert_eq!(tag, "nefor-tui");
}

#[test]
fn dir_override_replaces_existing_symlink_when_dir_changes() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());
    let first_dir = tempdir.path().join("first");
    let second_dir = tempdir.path().join("second");
    std::fs::create_dir_all(&first_dir).expect("mkdir first");
    std::fs::create_dir_all(&second_dir).expect("mkdir second");
    std::fs::write(first_dir.join("init.lua"), "return { which = 'first' }\n").expect("w1");
    std::fs::write(second_dir.join("init.lua"), "return { which = 'second' }\n").expect("w2");

    let lua = lua_with_pm();
    let _: bool = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "swap", dir = "{}" }} }})
            return true
            "#,
            first_dir.display()
        ))
        .eval()
        .expect("install 1");

    let link = tempdir.path().join("plugins").join("swap");
    assert_eq!(std::fs::read_link(&link).expect("link 1"), first_dir);

    // Re-run with a different dir: symlink must repoint.
    let lua2 = lua_with_pm();
    let which: String = lua2
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "swap", dir = "{}" }} }})
            return pm.load("swap").which
            "#,
            second_dir.display()
        ))
        .eval()
        .expect("install 2");
    assert_eq!(which, "second");
    assert_eq!(std::fs::read_link(&link).expect("link 2"), second_dir);
}

#[test]
fn dir_override_install_is_idempotent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());
    let plug_dir = tempdir.path().join("p");
    std::fs::create_dir_all(&plug_dir).expect("mkdir");
    std::fs::write(plug_dir.join("init.lua"), "return { ok = true }\n").expect("w");

    let lua = lua_with_pm();
    let _: bool = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "p", dir = "{}" }} }})
            return true
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("install 1");

    let link = tempdir.path().join("plugins").join("p");
    let mtime_before = std::fs::symlink_metadata(&link)
        .expect("stat 1")
        .modified()
        .expect("mtime 1");
    std::thread::sleep(std::time::Duration::from_millis(50));

    let lua2 = lua_with_pm();
    let _: bool = lua2
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "p", dir = "{}" }} }})
            return true
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("install 2");

    let mtime_after = std::fs::symlink_metadata(&link)
        .expect("stat 2")
        .modified()
        .expect("mtime 2");
    assert_eq!(mtime_before, mtime_after, "symlink must not be recreated");
}

#[test]
fn dir_override_refuses_to_clobber_non_symlink_entry() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());
    let plug_dir = tempdir.path().join("real");
    std::fs::create_dir_all(&plug_dir).expect("mkdir plug");
    std::fs::write(plug_dir.join("init.lua"), "return {}\n").expect("w");

    // Simulate a leftover from a prior clone-based install: a real
    // directory at <plugins_root>/<name>.
    let leftover = tempdir.path().join("plugins").join("real");
    std::fs::create_dir_all(&leftover).expect("mkdir leftover");

    let lua = lua_with_pm();
    let err = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "real", dir = "{}" }} }})
            "#,
            plug_dir.display()
        ))
        .exec()
        .expect_err("non-symlink leftover must error");
    assert!(
        err.to_string().contains("non-symlink entry already exists"),
        "got: {err}"
    );
}

#[test]
fn bin_resolves_default_and_named_binary() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let plug_dir = tempdir.path().join("openai-provider");
    let bin_dir = plug_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mkdir bin");
    let bin_a = bin_dir.join("openai-provider");
    let bin_b = bin_dir.join("openai-provider-helper");
    std::fs::write(&bin_a, b"fake").expect("write bin a");
    std::fs::write(&bin_b, b"fake").expect("write bin b");

    let lua = lua_with_pm();
    let (default, named): (String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm._internals.register("openai-provider", "{}")
            return pm.bin("openai-provider"),
                   pm.bin("openai-provider", "openai-provider-helper")
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("eval");
    assert_eq!(default, bin_a.display().to_string());
    assert_eq!(named, bin_b.display().to_string());
}

#[test]
fn bin_missing_raises() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let plug_dir = tempdir.path().join("noplug");
    std::fs::create_dir_all(plug_dir.join("bin")).expect("mkdir");

    let lua = lua_with_pm();
    let err = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm._internals.register("noplug", "{}")
            pm.bin("noplug")
            "#,
            plug_dir.display()
        ))
        .exec()
        .expect_err("missing bin must fail");
    let msg = err.to_string();
    assert!(msg.contains("not found"), "expected 'not found' in: {msg}");
}

// ---------------------------------------------------------------------------
// Sync tests — build_hash determinism
// ---------------------------------------------------------------------------

#[test]
fn build_hash_changes_with_tag_but_not_dir() {
    let lua = lua_with_pm();
    let (h1, h2, h3): (String, String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            -- Build function included so compute_build_hash actually runs.
            local b = function() end
            local h1 = pm._internals.compute_build_hash({
              name = "x", tag = "v1", build = b,
            })
            local h2 = pm._internals.compute_build_hash({
              name = "x", tag = "v1", build = b, dir = "/different",
            })
            local h3 = pm._internals.compute_build_hash({
              name = "x", tag = "v2", build = b,
            })
            return h1, h2, h3
            "#,
        )
        .eval()
        .expect("eval");
    assert_eq!(h1, h2, "dir must not affect build_hash");
    assert_ne!(h1, h3, "tag change must invalidate build_hash");
}

#[test]
fn build_hash_nil_when_no_build_function() {
    let lua = lua_with_pm();
    let v: Value = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            return pm._internals.compute_build_hash({ name = "x", tag = "v1" })
            "#,
        )
        .eval()
        .expect("eval");
    assert!(matches!(v, Value::Nil), "no build = no build_hash");
}

// ---------------------------------------------------------------------------
// Async install tests — drive against a local file:// git repo.
// ---------------------------------------------------------------------------

/// Create a self-contained git repo under `path` with a single commit on the
/// `main` branch. Returns the file:// URL safe to pass to `git clone`.
fn make_origin_repo(path: &std::path::Path) -> String {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(path).expect("mkdir origin");
    git(&["init", "--initial-branch=main", "--quiet"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    // Layout: lua-libs/ has a Lua module; rust-bin/ has unrelated content,
    // so sparse-checkout can prove it extracts only the requested subtree.
    std::fs::create_dir_all(path.join("lua-libs")).expect("mkdir lua-libs");
    std::fs::write(
        path.join("lua-libs").join("test-lib.lua"),
        "return { value = 42 }\n",
    )
    .expect("write test-lib.lua");
    std::fs::create_dir_all(path.join("rust-bin")).expect("mkdir rust-bin");
    std::fs::write(path.join("rust-bin").join("README"), "decoy\n").expect("write decoy");
    std::fs::write(path.join("README.md"), "root\n").expect("write root README");
    git(&["add", "."]);
    git(&["commit", "-m", "init", "--quiet"]);

    format!("file://{}", path.display())
}

fn run_git(path: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_git_isolated(path: &std::path::Path, args: &[&str], global_config: &std::path::Path) {
    let home = global_config.parent().expect("isolated Git home");
    let out = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .output()
        .expect("run isolated git");
    assert!(
        out.status.success(),
        "isolated git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn install_clones_and_creates_lockfile() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    let script = format!(
        r#"
        local pm = require("nefor-pm")
        pm.install({{
          {{ name = "test-plugin", url = "{}", branch = "main" }},
        }})
        return true
        "#,
        url
    );
    let ok: bool = lua.load(&script).eval().expect("install");
    assert!(ok);

    // The clone should exist with README.md present.
    let cloned = data
        .path()
        .join("plugins")
        .join("test-plugin")
        .join("README.md");
    assert!(cloned.exists(), "clone missing: {}", cloned.display());

    // Lockfile must contain the entry with a commit sha.
    let lockfile = data.path().join("plugins").join("nefor-pm.lock.json");
    let body = std::fs::read_to_string(&lockfile).expect("read lockfile");
    assert!(
        body.contains("\"test-plugin\""),
        "lock missing entry: {body}"
    );
    assert!(
        body.contains("\"ref\":\"main\""),
        "ref not recorded: {body}"
    );
}

#[test]
fn install_reproduces_existing_lock_without_moving_until_update() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    let pinned = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&origin)
        .output()
        .expect("read pinned commit");
    assert!(pinned.status.success());
    let pinned = String::from_utf8(pinned.stdout)
        .expect("utf8 commit")
        .trim()
        .to_owned();

    std::fs::write(origin.join("README.md"), "new head\n").expect("update README");
    run_git(&origin, &["add", "README.md"]);
    run_git(&origin, &["commit", "-m", "new head", "--quiet"]);
    let moved = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&origin)
        .output()
        .expect("read moved commit");
    assert!(moved.status.success());
    let moved = String::from_utf8(moved.stdout)
        .expect("utf8 commit")
        .trim()
        .to_owned();

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let plugins = data.path().join("plugins");
    std::fs::create_dir_all(&plugins).expect("plugins dir");
    std::fs::write(
        plugins.join("nefor-pm.lock.json"),
        format!("{{\"pinned\":{{\"ref\":\"main\",\"commit\":\"{pinned}\"}}}}\n"),
    )
    .expect("seed lock");

    let lua = lua_with_pm();
    let install = format!(
        r#"local pm = require("nefor-pm"); pm.install({{{{ name = "pinned", url = "{}", branch = "main" }}}})"#,
        url
    );
    lua.load(&install).exec().expect("fresh install from lock");

    let checkout = plugins.join("pinned");
    let checkout_head = || {
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&checkout)
            .output()
            .expect("read checkout head");
        assert!(head.status.success());
        String::from_utf8_lossy(&head.stdout).trim().to_owned()
    };
    assert_eq!(checkout_head(), pinned, "fresh install must honor lock");
    lua.load(&install).exec().expect("ordinary sync");
    assert_eq!(checkout_head(), pinned, "ordinary sync must not move lock");

    let update = format!(
        r#"
        local pm = require("nefor-pm")
        local real_run = nefor.process.run
        local events = {{}}
        nefor.process.run = function(opts)
          if opts.cmd == "git" and opts.args[3] == "fetch" then
            assert(events[#events] == "pinned: Fetching revision main")
          elseif opts.cmd == "git" and opts.args[3] == "checkout" then
            assert(events[#events] == "pinned: Checking out main")
          end
          return real_run(opts)
        end
        pm.update({{{{ name = "pinned", url = "{}", branch = "main" }}}}, {{
          on_progress = function(name, phase)
            events[#events + 1] = name .. ": " .. phase
          end,
        }})
        return table.concat(events, "|")
        "#,
        url
    );
    let update_events: String = lua.load(&update).eval().expect("explicit update");
    assert_eq!(
        update_events,
        "pinned: Fetching revision main|pinned: Checking out main|pinned: Ready"
    );
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&checkout)
        .output()
        .expect("read checkout head");
    assert!(head.status.success());
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), moved);
    let lock = std::fs::read_to_string(plugins.join("nefor-pm.lock.json")).expect("lock");
    assert!(
        lock.contains(&moved),
        "explicit update must move lock: {lock}"
    );
}

#[test]
fn install_uses_valid_pin_offline_and_rejects_invalid_pin() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let lua = lua_with_pm();
    let install = format!(
        r#"local pm = require("nefor-pm"); pm.install({{{{ name = "offline", url = "{}", branch = "main" }}}})"#,
        url
    );
    lua.load(&install).exec().expect("initial install");

    std::fs::remove_dir_all(&origin).expect("remove origin to simulate offline");
    lua.load(&install)
        .exec()
        .expect("exact local pin must not fetch while offline");

    let checkout = data.path().join("plugins").join("offline");
    run_git(&checkout, &["checkout", "--detach", "HEAD~0"]);
    std::fs::write(checkout.join("README.md"), "dirty but same commit\n").expect("dirty checkout");
    run_git(&checkout, &["add", "README.md"]);
    run_git(&checkout, &["config", "user.email", "test@example.com"]);
    run_git(&checkout, &["config", "user.name", "Test"]);
    run_git(
        &checkout,
        &["commit", "-m", "wrong local commit", "--quiet"],
    );
    let err = lua
        .load(&install)
        .exec()
        .expect_err("wrong local commit with no origin must fail loudly");
    assert!(err.to_string().contains("nefor-pm[offline]"), "{err}");
}

#[test]
fn sync_checkout_updates_existing_checkout_to_requested_tag() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    run_git(&origin, &["tag", "v1.0.0"]);

    std::fs::write(origin.join("README.md"), "tag-two\n").expect("update README");
    run_git(&origin, &["add", "README.md"]);
    run_git(&origin, &["commit", "-m", "tag two", "--quiet"]);
    run_git(&origin, &["tag", "v1.0.1"]);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let checkout = data.path().join("managed-upstream");

    let lua = lua_with_pm();
    let (first_ref, second_ref, first_body, second_body): (String, String, String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            local function read_readme(dir)
              local f = assert(io.open(dir .. "/README.md", "r"))
              local body = f:read("*a")
              f:close()
              return body
            end
            local dir = "{}"
            local first = pm.sync_checkout({{
              name = "managed-upstream",
              dir = dir,
              url = "{}",
              ref = "v1.0.0",
              ref_kind = "tag",
            }})
            local first_body = read_readme(dir)
            local second = pm.sync_checkout({{
              name = "managed-upstream",
              dir = dir,
              url = "{}",
              ref = "v1.0.1",
              ref_kind = "tag",
            }})
            local second_body = read_readme(dir)
            return first.ref, second.ref, first_body, second_body
            "#,
            checkout.display(),
            url,
            url
        ))
        .eval()
        .expect("sync checkout");

    assert_eq!(first_ref, "v1.0.0");
    assert_eq!(second_ref, "v1.0.1");
    assert_eq!(first_body, "root\n");
    assert_eq!(second_body, "tag-two\n");
}

#[test]
fn sync_checkout_resolves_unpushed_local_commit_and_keeps_authoritative_pin_offline() {
    let work = tempfile::tempdir().expect("workdir");
    let source = work.path().join("source");
    let _url = make_origin_repo(&source);
    let first = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&source)
        .output()
        .expect("read first commit");
    assert!(first.status.success());
    let first = String::from_utf8_lossy(&first.stdout).trim().to_owned();

    std::fs::write(source.join("README.md"), "local-only second commit\n").expect("update");
    run_git(&source, &["add", "README.md"]);
    run_git(&source, &["commit", "-m", "local only", "--quiet"]);
    let second = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&source)
        .output()
        .expect("read second commit");
    assert!(second.status.success());
    let second = String::from_utf8_lossy(&second.stdout).trim().to_owned();

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let checkout = data.path().join("managed-nefor");
    let lockfile = data.path().join("nefor.commit");
    let lua = lua_with_pm();
    let sync = |commit: &str, update: bool| {
        let operation = if update {
            "update_checkout"
        } else {
            "sync_checkout"
        };
        lua.load(format!(
            r#"
            local pm = require("nefor-pm")
            local resolved = pm.{operation}({{
              name = "managed-nefor",
              dir = "{}",
              url = "{}",
              ref = "{commit}",
              ref_kind = "commit",
              lockfile = "{}",
            }})
            return resolved.dir, resolved.commit, resolved.head
            "#,
            checkout.display(),
            source.display(),
            lockfile.display(),
        ))
        .eval::<(String, String, String)>()
    };

    let (resolved_dir, commit, head) = sync(&first, false).expect("resolve local commit");
    assert_eq!(resolved_dir, checkout.to_string_lossy());
    assert_eq!(commit, first);
    assert_eq!(
        head, first,
        "reported identity must be verified checkout HEAD"
    );

    let (_, commit, _) = sync(&second, false).expect("ordinary sync keeps pin");
    assert_eq!(
        commit, first,
        "ordinary sync must not move authoritative pin"
    );

    std::fs::remove_dir_all(&checkout).expect("remove managed checkout");
    let (_, commit, head) =
        sync(&second, false).expect("fresh checkout reproduces authoritative pin");
    assert_eq!(commit, first);
    assert_eq!(head, first);

    std::fs::rename(&source, work.path().join("source-offline")).expect("hide source");
    let (_, commit, head) = sync(&second, false).expect("reuse exact checkout offline");
    assert_eq!(commit, first);
    assert_eq!(head, first);

    std::fs::rename(work.path().join("source-offline"), &source).expect("restore source");
    let (_, commit, head) = sync(&second, true).expect("explicit update moves pin");
    assert_eq!(commit, second);
    assert_eq!(head, second);
    assert_eq!(
        std::fs::read_to_string(lockfile).expect("read lock").trim(),
        second
    );
}

#[test]
fn install_sparse_checkout_pulls_only_subtree() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    let script = format!(
        r#"
        local pm = require("nefor-pm")
        pm.install({{
          {{ name = "subtree-only", url = "{}", branch = "main", path = "lua-libs/" }},
        }})
        return true
        "#,
        url
    );
    let ok: bool = lua.load(&script).eval().expect("install");
    assert!(ok);

    let plug_root = data.path().join("plugins").join("subtree-only");
    // Post-flatten: contents of `lua-libs/` live directly at <plug_root>/.
    // The subtree path component is removed so `package.path`'s
    // `<plugins_root>/?/init.lua` graft resolves correctly.
    let flat = plug_root.join("test-lib.lua");
    let nested = plug_root.join("lua-libs").join("test-lib.lua");
    let decoy = plug_root.join("rust-bin").join("README");
    let dotgit = plug_root.join(".git");
    assert!(
        flat.exists(),
        "flattened lua file missing: {}",
        flat.display()
    );
    assert!(
        !nested.exists(),
        "subtree path component not removed: {}",
        nested.display()
    );
    assert!(
        !decoy.exists(),
        "sparse-checkout leaked rust-bin/: {}",
        decoy.display()
    );
    assert!(
        dotgit.exists(),
        ".git was disturbed by flatten: {}",
        dotgit.display()
    );
}

/// Build a repo whose contents live deep in a path
/// (`subdir/deep/lib/init.lua`). Mirrors how real plugins ship inside the
/// nefor monorepo (e.g. `plugins/openai-provider/lua/openai-provider/`).
fn make_deep_origin_repo(path: &std::path::Path) -> String {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(path).expect("mkdir origin");
    git(&["init", "--initial-branch=main", "--quiet"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    let deep = path.join("subdir").join("deep").join("lib");
    std::fs::create_dir_all(&deep).expect("mkdir deep");
    std::fs::write(deep.join("init.lua"), "return { name = 'deep-lib' }\n")
        .expect("write init.lua");
    std::fs::write(deep.join("helper.lua"), "return { tag = 'helper' }\n")
        .expect("write helper.lua");
    std::fs::write(path.join("README.md"), "root\n").expect("write README");
    git(&["add", "."]);
    git(&["commit", "-m", "init", "--quiet"]);
    format!("file://{}", path.display())
}

#[test]
fn install_flattens_deeply_nested_sparse_subtree() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_deep_origin_repo(&origin);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    let script = format!(
        r#"
        local pm = require("nefor-pm")
        pm.install({{
          {{ name = "deep-lib", url = "{}", branch = "main",
             path = "subdir/deep/lib/" }},
        }})
        local mod = pm.load("deep-lib")
        return mod.name
        "#,
        url
    );
    let name: String = lua.load(&script).eval().expect("install");
    assert_eq!(
        name, "deep-lib",
        "require('deep-lib') must resolve to init.lua flattened up from the subtree"
    );

    let plug_root = data.path().join("plugins").join("deep-lib");
    // Files are flat: <plug_root>/init.lua, <plug_root>/helper.lua.
    assert!(
        plug_root.join("init.lua").exists(),
        "init.lua not at flat path: {}",
        plug_root.join("init.lua").display()
    );
    assert!(
        plug_root.join("helper.lua").exists(),
        "helper.lua not at flat path"
    );
    // Intermediate path components are gone.
    assert!(
        !plug_root.join("subdir").exists(),
        "intermediate subdir/ should be removed"
    );
    // .git survives so subsequent updates work.
    assert!(
        plug_root.join(".git").exists(),
        ".git was disturbed by flatten"
    );
    // Re-running install is idempotent: the flat layout stays in place.
    let ok: bool = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{
              {{ name = "deep-lib", url = "{}", branch = "main",
                 path = "subdir/deep/lib/" }},
            }})
            return pm.load("deep-lib").name == "deep-lib"
            "#,
            url
        ))
        .eval()
        .expect("re-install");
    assert!(ok, "idempotent re-install must keep deep-lib loadable");
}

#[test]
fn install_runs_build_callback_and_records_hash() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    // Outer raw string uses `r##"..."##` so the embedded `"#!/bin/sh` doesn't
    // close it prematurely (`r#"..."#` would).
    let script = format!(
        r##"
        local pm = require("nefor-pm")
        local invocations = 0
        local function build(plugin)
          invocations = invocations + 1
          -- Pretend to compile by writing a fake binary into plugin.dir/bin/<name>.
          local f = io.open(plugin.dir .. "/bin/" .. plugin.name, "w")
          if not f then error("open bin failed") end
          f:write("#!/bin/sh\necho built\n")
          f:close()
          _G._last_plugin_dir = plugin.dir
          _G._last_plugin_tag = plugin.tag
        end
        pm.install({{
          {{ name = "with-build", url = "{}", branch = "main", build = build }},
        }})
        _G._first_invocation_count = invocations
        local first_bin = pm.bin("with-build")
        -- Run install again — build_hash unchanged, fresh clone present →
        -- build must NOT re-run.
        pm.install({{
          {{ name = "with-build", url = "{}", branch = "main", build = build }},
        }})
        _G._second_invocation_count = invocations
        return first_bin
        "##,
        url, url
    );
    let bin_path: String = lua.load(&script).eval().expect("install");

    assert!(
        std::path::Path::new(&bin_path).exists(),
        "build artefact missing: {bin_path}"
    );
    let first: i64 = lua
        .globals()
        .get("_first_invocation_count")
        .expect("first count");
    let second: i64 = lua
        .globals()
        .get("_second_invocation_count")
        .expect("second count");
    assert_eq!(first, 1, "build must run on first install");
    assert_eq!(
        second, 1,
        "build must be skipped on second idempotent install"
    );
    let last_dir: String = lua.globals().get("_last_plugin_dir").expect("plugin.dir");
    assert!(
        last_dir.ends_with("plugins/with-build"),
        "plugin.dir = {last_dir}"
    );
    let last_tag: String = lua.globals().get("_last_plugin_tag").expect("plugin.tag");
    assert_eq!(last_tag, "main");
}

#[test]
fn install_reports_download_before_clone_and_never_reports_ready_after_clone_failure() {
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let lua = lua_with_pm();

    let (message, events): (String, String) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local events = {}
            nefor.process.run = function(opts)
              if opts.cmd == "git" and opts.args[1] == "clone" then
                assert(events[1] == "blocked: Downloading package")
                return { code = 19, stdout = "", stderr = "fixture clone blocked" }
              end
              error("unexpected process: " .. tostring(opts.cmd))
            end
            local ok, err = pcall(function()
              pm.install({
                { name = "blocked", url = "https://invalid.example/plugin.git", branch = "main" },
              }, {
                on_progress = function(name, phase)
                  events[#events + 1] = name .. ": " .. phase
                end,
              })
            end)
            assert(not ok)
            return tostring(err), table.concat(events, "|")
            "#,
        )
        .eval()
        .expect("exercise failed clone");

    assert!(message.contains("fixture clone blocked"), "{message}");
    assert_eq!(events, "blocked: Downloading package");
}

#[test]
fn install_reports_cold_phases_and_ready_only_after_lock_is_saved() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let lua = lua_with_pm();

    let (cold, cached): (String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            local events = {{}}
            local function build(_) end
            local opts = {{
              on_progress = function(name, phase)
                if phase == "Ready" then
                  assert(nefor.fs.exists(nefor.fs.data_root() .. "/plugins/nefor-pm.lock.json"))
                end
                events[#events + 1] = name .. ": " .. phase
              end,
            }}
            local specs = {{
              {{ name = "phased", url = "{}", branch = "main", build = build }},
            }}
            pm.install(specs, opts)
            local cold = table.concat(events, "|")
            events = {{}}
            pm.install(specs, opts)
            return cold, table.concat(events, "|")
            "#,
            url
        ))
        .eval()
        .expect("install twice");

    assert_eq!(
        cold,
        "phased: Downloading package|phased: Building package|phased: Ready"
    );
    assert_eq!(cached, "", "a fully cached install must not report work");
}

#[test]
fn install_does_not_report_ready_when_build_fails() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let lua = lua_with_pm();

    let (message, events): (String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            local events = {{}}
            local ok, err = pcall(function()
              pm.install({{
                {{
                  name = "broken-build",
                  url = "{}",
                  branch = "main",
                  build = function() error("fixture compiler failure") end,
                }},
              }}, {{
                on_progress = function(name, phase)
                  events[#events + 1] = name .. ": " .. phase
                end,
              }})
            end)
            assert(not ok)
            return tostring(err), table.concat(events, "|")
            "#,
            url
        ))
        .eval()
        .expect("exercise failed build");

    assert!(message.contains("fixture compiler failure"), "{message}");
    assert_eq!(
        events,
        "broken-build: Downloading package|broken-build: Building package"
    );
}

#[test]
fn stderr_progress_writes_and_flushes_one_line_without_touching_stdout() {
    let lua = lua_with_pm();
    let (line, flushes, stdout_writes): (String, i64, i64) = lua
        .load(
            r#"
            local pm = require("nefor-pm")
            local stderr = { text = "", flushes = 0 }
            function stderr:write(value) self.text = self.text .. value end
            function stderr:flush() self.flushes = self.flushes + 1 end
            local stdout = { writes = 0 }
            function stdout:write(_) self.writes = self.writes + 1 end
            io.stderr = stderr
            io.stdout = stdout
            pm.stderr_progress("fixture", "Compiling classifier (Cargo)")
            return stderr.text, stderr.flushes, stdout.writes
            "#,
        )
        .eval()
        .expect("report progress");

    assert_eq!(line, "[nefor-pm] fixture: Compiling classifier (Cargo)\n");
    assert_eq!(flushes, 1);
    assert_eq!(stdout_writes, 0);
}

#[test]
fn managed_da_package_materializes_lfs_builds_and_resolves_private_binary() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);
    std::fs::create_dir_all(origin.join("classifier")).expect("classifier dir");
    std::fs::write(
        origin.join("classifier/model.onnx"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:test\nsize 1048576\n",
    )
    .expect("model pointer");
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-m", "add model pointer", "--quiet"]);
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&origin)
        .output()
        .expect("rev-parse fixture")
        .stdout;
    let commit = String::from_utf8(commit)
        .expect("utf8 commit")
        .trim()
        .to_owned();

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let lua = lua_with_pm();
    let script = format!(
        r#"
        local pm = require("nefor-pm")
        local da = require("libs.tool-validator.da")
        local real_run = nefor.process.run
        local lfs_calls, cargo_calls = 0, 0
        local events = {{}}
        nefor.process.run = function(opts)
          if opts.cmd == "git" and opts.args[1] == "lfs" then
            assert(events[#events] == "fixture-da: Downloading classifier model (Git LFS)")
            lfs_calls = lfs_calls + 1
            if opts.args[2] == "install" then
              assert(opts.args[3] == "--local")
              assert(opts.args[4] == "--skip-repo")
            elseif opts.args[2] == "pull" then
              assert(opts.args[3] == "--include")
              assert(opts.args[4] == "classifier/model.onnx")
              assert(opts.args[5] == "--exclude")
              assert(opts.args[6] == "")
              local f = assert(io.open(opts.cwd .. "/classifier/model.onnx", "wb"))
              f:write(string.rep("m", 1024 * 1024))
              f:close()
            else
              error("unexpected Git LFS command")
            end
            return {{ code = 0, stdout = "", stderr = "" }}
          end
          if opts.cmd == "cargo" then
            assert(events[#events] == "fixture-da: Compiling classifier (Cargo)")
            cargo_calls = cargo_calls + 1
            local root
            for i, arg in ipairs(opts.args) do
              if arg == "--root" then root = opts.args[i + 1] end
            end
            if root == "." then root = opts.cwd end
            local f = assert(io.open(root .. "/bin/da", "wb"))
            f:write("fixture da")
            f:close()
            return {{ code = 0, stdout = "", stderr = "" }}
          end
          return real_run(opts)
        end
        pm.install({{ da.package {{ name = "fixture-da", url = "{}", commit = "{}" }} }}, {{
          on_progress = function(name, phase)
            events[#events + 1] = name .. ": " .. phase
          end,
        }})
        return pm.bin("fixture-da", "da"), lfs_calls, cargo_calls, table.concat(events, "|")
        "#,
        url, commit
    );
    let (binary, lfs_calls, cargo_calls, events): (String, i64, i64, String) =
        lua.load(script).eval().expect("install managed da fixture");
    assert_eq!(
        binary,
        data.path()
            .join("plugins/fixture-da/bin/da")
            .to_string_lossy()
    );
    assert_eq!(
        lfs_calls, 2,
        "local LFS filters must be prepared before materialization"
    );
    assert_eq!(cargo_calls, 1, "private package build must run once");
    assert_eq!(
        events,
        concat!(
            "fixture-da: Downloading package|",
            "fixture-da: Fetching revision ",
            "{}|",
            "fixture-da: Checking out ",
            "{}|",
            "fixture-da: Building package|",
            "fixture-da: Downloading classifier model (Git LFS)|",
            "fixture-da: Compiling classifier (Cargo)|",
            "fixture-da: Ready"
        )
        .replace("{}", &commit)
    );
}

#[test]
fn managed_da_package_reports_missing_git_lfs_prerequisite() {
    let fixture = tempfile::tempdir().expect("fixture");
    std::fs::create_dir_all(fixture.path().join("classifier")).expect("classifier dir");
    std::fs::write(
        fixture.path().join("classifier/model.onnx"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:test\nsize 1048576\n",
    )
    .expect("model pointer");

    let lua = lua_with_pm();
    lua.globals()
        .set("fixture_root", fixture.path().to_string_lossy().as_ref())
        .expect("fixture root");
    let error = lua
        .load(
            r#"
            local da = require("libs.tool-validator.da")
            nefor.process.run = function(_)
              return { code = -1, stderr = "spawn failed: git-lfs unavailable" }
            end
            da._internals.build { dir = fixture_root, name = "da" }
            "#,
        )
        .exec()
        .expect_err("missing Git LFS must fail");
    let message = error.to_string();
    assert!(message.contains("install Git LFS"), "{message}");
    assert!(message.contains("git-lfs unavailable"), "{message}");
}

#[test]
fn managed_da_package_preserves_success_output_when_pointer_remains() {
    let fixture = tempfile::tempdir().expect("fixture");
    std::fs::create_dir_all(fixture.path().join("classifier")).expect("classifier dir");
    std::fs::write(
        fixture.path().join("classifier/model.onnx"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:test\nsize 1048576\n",
    )
    .expect("model pointer");

    let lua = lua_with_pm();
    lua.globals()
        .set("fixture_root", fixture.path().to_string_lossy().as_ref())
        .expect("fixture root");
    let error = lua
        .load(
            r#"
            local da = require("libs.tool-validator.da")
            nefor.process.run = function(opts)
              if opts.args[2] == "install" then
                return { code = 0, stdout = "install-out", stderr = "install-err" }
              end
              return { code = 0, stdout = "pull-out", stderr = "pull-err" }
            end
            da._internals.build { dir = fixture_root, name = "da" }
            "#,
        )
        .exec()
        .expect_err("unchanged pointer must fail");
    let message = error.to_string();
    assert!(message.contains("Git LFS did not materialize"), "{message}");
    for expected in ["install-out", "install-err", "pull-out", "pull-err"] {
        assert!(
            message.contains(expected),
            "missing {expected:?}: {message}"
        );
    }
}

#[test]
fn managed_da_package_initializes_lfs_and_overrides_fetch_exclusion() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = fixture.path().join("source");
    let checkout = fixture.path().join("checkout");
    let global_config = fixture.path().join("isolated-global.gitconfig");
    std::fs::write(&global_config, "").expect("empty global git config");

    std::fs::create_dir_all(source.join("classifier")).expect("classifier dir");
    run_git_isolated(
        &source,
        &["init", "--initial-branch=main", "--quiet"],
        &global_config,
    );
    run_git_isolated(
        &source,
        &["config", "user.email", "test@example.com"],
        &global_config,
    );
    run_git_isolated(&source, &["config", "user.name", "Test"], &global_config);
    run_git_isolated(
        &source,
        &["lfs", "install", "--local", "--skip-repo"],
        &global_config,
    );
    run_git_isolated(
        &source,
        &["lfs", "track", "classifier/*.onnx"],
        &global_config,
    );
    let model = b"small local classifier fixture\n";
    std::fs::write(source.join("classifier/model.onnx"), model).expect("model content");
    run_git_isolated(
        &source,
        &["add", ".gitattributes", "classifier/model.onnx"],
        &global_config,
    );
    run_git_isolated(
        &source,
        &["commit", "-m", "add LFS fixture", "--quiet"],
        &global_config,
    );

    let clone = Command::new("git")
        .args([
            "clone",
            &format!("file://{}", source.display()),
            checkout.to_string_lossy().as_ref(),
        ])
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("HOME", fixture.path())
        .env("XDG_CONFIG_HOME", fixture.path().join("xdg"))
        .output()
        .expect("clone LFS fixture");
    assert!(
        clone.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    run_git_isolated(
        &checkout,
        &["config", "lfs.fetchexclude", "classifier/model.onnx"],
        &global_config,
    );
    assert!(
        std::fs::read_to_string(checkout.join("classifier/model.onnx"))
            .expect("pointer before build")
            .starts_with("version https://git-lfs.github.com/spec/v1")
    );

    let lua = lua_with_pm();
    lua.globals()
        .set("fixture_root", checkout.to_string_lossy().as_ref())
        .expect("fixture root");
    lua.globals()
        .set(
            "fixture_global_config",
            global_config.to_string_lossy().as_ref(),
        )
        .expect("global config");
    lua.globals()
        .set("fixture_home", fixture.path().to_string_lossy().as_ref())
        .expect("fixture home");
    lua.globals()
        .set(
            "fixture_xdg_config_home",
            fixture.path().join("xdg").to_string_lossy().as_ref(),
        )
        .expect("fixture XDG config home");
    lua.load(
        r#"
        local da = require("libs.tool-validator.da")
        local real_run = nefor.process.run
        nefor.process.run = function(opts)
          if opts.cmd == "git" then
            opts.env = {
              GIT_CONFIG_GLOBAL = fixture_global_config,
              GIT_CONFIG_NOSYSTEM = "1",
              HOME = fixture_home,
              XDG_CONFIG_HOME = fixture_xdg_config_home,
            }
            local result = real_run(opts)
            if opts.args[2] == "install" then
              local hook = io.open(opts.cwd .. "/.git/hooks/pre-push", "rb")
              assert(hook == nil, "repository-local filter setup installed a hook")
            end
            return result
          elseif opts.cmd == "cargo" then
            return { code = 0, stdout = "", stderr = "" }
          end
          return real_run(opts)
        end
        da._internals.build { dir = fixture_root, name = "da" }
        "#,
    )
    .exec()
    .expect("materialize real local LFS fixture");

    assert_eq!(
        std::fs::read(checkout.join("classifier/model.onnx")).expect("materialized model"),
        model
    );
    let process_filter = Command::new("git")
        .args(["config", "--local", "--get", "filter.lfs.process"])
        .current_dir(&checkout)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", fixture.path())
        .env("XDG_CONFIG_HOME", fixture.path().join("xdg"))
        .output()
        .expect("read local LFS filter");
    assert!(process_filter.status.success());
    assert_eq!(
        String::from_utf8_lossy(&process_filter.stdout).trim(),
        "git-lfs filter-process"
    );

    let pointer = Command::new("git")
        .args(["show", "HEAD:classifier/model.onnx"])
        .current_dir(&checkout)
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", fixture.path())
        .env("XDG_CONFIG_HOME", fixture.path().join("xdg"))
        .output()
        .expect("read committed pointer");
    assert!(pointer.status.success());
    let mut dirty_pointer = pointer.stdout;
    dirty_pointer.extend_from_slice(b"# local modification\n");
    std::fs::write(checkout.join("classifier/model.onnx"), &dirty_pointer)
        .expect("write modified pointer");

    let dirty_lua = lua_with_pm();
    dirty_lua
        .globals()
        .set("fixture_root", checkout.to_string_lossy().as_ref())
        .expect("fixture root");
    dirty_lua
        .globals()
        .set(
            "fixture_global_config",
            global_config.to_string_lossy().as_ref(),
        )
        .expect("global config");
    dirty_lua
        .globals()
        .set("fixture_home", fixture.path().to_string_lossy().as_ref())
        .expect("fixture home");
    dirty_lua
        .globals()
        .set(
            "fixture_xdg_config_home",
            fixture.path().join("xdg").to_string_lossy().as_ref(),
        )
        .expect("fixture XDG config home");
    let dirty_error = dirty_lua
        .load(
            r#"
            local da = require("libs.tool-validator.da")
            local real_run = nefor.process.run
            nefor.process.run = function(opts)
              if opts.cmd == "git" then
                opts.env = {
                  GIT_CONFIG_GLOBAL = fixture_global_config,
                  GIT_CONFIG_NOSYSTEM = "1",
                  HOME = fixture_home,
                  XDG_CONFIG_HOME = fixture_xdg_config_home,
                }
                return real_run(opts)
              end
              error("dirty pointer reached compiler")
            end
            da._internals.build { dir = fixture_root, name = "da" }
            "#,
        )
        .exec()
        .expect_err("Git LFS must not overwrite a modified pointer");
    assert!(
        dirty_error
            .to_string()
            .contains("Git LFS did not materialize"),
        "{dirty_error}"
    );
    assert_eq!(
        std::fs::read(checkout.join("classifier/model.onnx")).expect("dirty pointer remains"),
        dirty_pointer,
        "the helper must not force-overwrite a modified model"
    );
    assert_eq!(
        std::fs::read(&global_config).expect("read isolated global config"),
        b"",
        "Git LFS setup must not mutate global configuration"
    );
}

#[test]
fn install_skips_clone_when_lockfile_matches_head() {
    let work = tempfile::tempdir().expect("workdir");
    let origin = work.path().join("origin");
    let url = make_origin_repo(&origin);

    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    let install_script = format!(
        r#"
        local pm = require("nefor-pm")
        pm.install({{
          {{ name = "twice", url = "{}", branch = "main" }},
        }})
        return true
        "#,
        url
    );
    let _: bool = lua.load(&install_script).eval().expect("install 1");

    let plug_dir = data.path().join("plugins").join("twice");
    let head_path = plug_dir.join(".git").join("HEAD");
    let mtime_before = std::fs::metadata(&head_path)
        .expect("stat head")
        .modified()
        .expect("mtime");

    // Sleep a beat so a second clone would produce a distinguishable mtime.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let _: bool = lua.load(&install_script).eval().expect("install 2");
    let mtime_after = std::fs::metadata(&head_path)
        .expect("stat head 2")
        .modified()
        .expect("mtime");
    assert_eq!(
        mtime_before, mtime_after,
        ".git/HEAD mtime must not change on idempotent re-install"
    );
}

#[test]
fn install_invalid_lockfile_fails_loudly() {
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());
    let plugins = data.path().join("plugins");
    std::fs::create_dir_all(&plugins).expect("plugins dir");
    std::fs::write(plugins.join("nefor-pm.lock.json"), "not json\n").expect("lock");
    let lua = lua_with_pm();
    let err = lua
        .load(r#"local pm = require("nefor-pm"); pm.install({})"#)
        .exec()
        .expect_err("invalid lock must fail");
    assert!(err.to_string().contains("lockfile is invalid"), "{err}");
}

#[test]
fn install_failed_clone_surfaces_structured_error() {
    let data = tempfile::tempdir().expect("datadir");
    let _g = DataDirGuard::new(data.path());

    let lua = lua_with_pm();
    // file:// URL pointing at a path with no repo — `git clone` exits
    // non-zero. pm wraps the data in a clean `nefor-pm[<name>]: ...` error.
    let bogus = data.path().join("definitely-not-a-repo");
    let url = format!("file://{}", bogus.display());
    let err = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{
              {{ name = "broken", url = "{}", branch = "main" }},
            }})
            "#,
            url
        ))
        .exec()
        .expect_err("clone of missing repo must error");
    let msg = err.to_string();
    assert!(
        msg.contains("nefor-pm[broken]"),
        "error must be labelled by name: {msg}"
    );
    assert!(
        msg.contains("git exited"),
        "error must surface the structured exit: {msg}"
    );
}

// ---------------------------------------------------------------------------
// pm.load — dotted requires resolve through Lua's normal package.path search
// once pm.install has grafted the plugin's parent dir.
// ---------------------------------------------------------------------------

#[test]
fn load_resolves_submodule_under_plugin_dir() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _g = DataDirGuard::new(tempdir.path());
    let plug_dir = tempdir.path().join("nefor-libs");
    std::fs::create_dir_all(&plug_dir).expect("mkdir plugin root");
    std::fs::write(plug_dir.join("subm.lua"), "return { tag = 'subm' }\n").expect("write subm");
    std::fs::write(plug_dir.join("init.lua"), "return { tag = 'root' }\n")
        .expect("write root init");

    let lua = lua_with_pm();
    let (a, b): (String, String) = lua
        .load(format!(
            r#"
            local pm = require("nefor-pm")
            pm.install({{ {{ name = "nefor-libs", dir = "{}" }} }})
            local root = pm.load("nefor-libs")
            local sub  = pm.load("nefor-libs.subm")
            return root.tag, sub.tag
            "#,
            plug_dir.display()
        ))
        .eval()
        .expect("eval");
    assert_eq!(a, "root");
    assert_eq!(b, "subm");
}
