//! Tests for `lua/nefor-pm/init.lua`.
//!
//! Post-sync refactor every test runs on a plain `Lua::new()` /
//! `eval()` harness — pm.install drives `nefor.process.run` and
//! `nefor.fs.*` synchronously, so the tokio multi-thread runtime
//! the previous version needed is gone.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use mlua::{Lua, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Resolve `<repo-root>/lua/`. CARGO_MANIFEST_DIR points at crates/nefor,
/// so we walk up two levels.
fn lua_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root two levels above crates/nefor")
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
    nefor::lua::bindings::install_process(lua, &nefor)?;
    // pm's data_root() now delegates to `nefor.fs.data_root()` — capture
    // the resolved value from NEFOR_DATA_DIR (the DataDirGuard sets it
    // before constructing this VM). When unset, the resolver falls back
    // to XDG/HOME, but pm tests always pin the env so the unset branch
    // is moot here.
    let data_dir_path = std::env::var("NEFOR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/empty/nefor-pm-test-data"));
    nefor::lua::bindings::install_fs(lua, &nefor, nefor::paths::DataDir(data_dir_path))?;
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
    assert!(
        err.to_string().contains("non-empty string"),
        "got: {err}"
    );
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
    assert!(alpha_pos < mid_pos && mid_pos < zeta_pos, "keys must be sorted; got {body}");

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
    std::fs::write(
        plug_dir.join("init.lua"),
        "return { tag = 'nefor-tui' }\n",
    )
    .expect("write");

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
        assert!(out.status.success(), "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr));
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
    let cloned = data.path().join("plugins").join("test-plugin").join("README.md");
    assert!(cloned.exists(), "clone missing: {}", cloned.display());

    // Lockfile must contain the entry with a commit sha.
    let lockfile = data.path().join("plugins").join("nefor-pm.lock.json");
    let body = std::fs::read_to_string(&lockfile).expect("read lockfile");
    assert!(body.contains("\"test-plugin\""), "lock missing entry: {body}");
    assert!(body.contains("\"ref\":\"main\""), "ref not recorded: {body}");
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
    assert!(flat.exists(), "flattened lua file missing: {}", flat.display());
    assert!(!nested.exists(),
        "subtree path component not removed: {}", nested.display());
    assert!(!decoy.exists(),
        "sparse-checkout leaked rust-bin/: {}", decoy.display());
    assert!(dotgit.exists(),
        ".git was disturbed by flatten: {}", dotgit.display());
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
        assert!(out.status.success(), "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir_all(path).expect("mkdir origin");
    git(&["init", "--initial-branch=main", "--quiet"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    let deep = path.join("subdir").join("deep").join("lib");
    std::fs::create_dir_all(&deep).expect("mkdir deep");
    std::fs::write(
        deep.join("init.lua"),
        "return { name = 'deep-lib' }\n",
    )
    .expect("write init.lua");
    std::fs::write(
        deep.join("helper.lua"),
        "return { tag = 'helper' }\n",
    )
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
    assert_eq!(name, "deep-lib",
        "require('deep-lib') must resolve to init.lua flattened up from the subtree");

    let plug_root = data.path().join("plugins").join("deep-lib");
    // Files are flat: <plug_root>/init.lua, <plug_root>/helper.lua.
    assert!(plug_root.join("init.lua").exists(),
        "init.lua not at flat path: {}", plug_root.join("init.lua").display());
    assert!(plug_root.join("helper.lua").exists(),
        "helper.lua not at flat path");
    // Intermediate path components are gone.
    assert!(!plug_root.join("subdir").exists(),
        "intermediate subdir/ should be removed");
    // .git survives so subsequent updates work.
    assert!(plug_root.join(".git").exists(), ".git was disturbed by flatten");
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

    assert!(std::path::Path::new(&bin_path).exists(), "build artefact missing: {bin_path}");
    let first: i64 = lua.globals().get("_first_invocation_count").expect("first count");
    let second: i64 = lua.globals().get("_second_invocation_count").expect("second count");
    assert_eq!(first, 1, "build must run on first install");
    assert_eq!(second, 1, "build must be skipped on second idempotent install");
    let last_dir: String = lua.globals().get("_last_plugin_dir").expect("plugin.dir");
    assert!(last_dir.ends_with("plugins/with-build"), "plugin.dir = {last_dir}");
    let last_tag: String = lua.globals().get("_last_plugin_tag").expect("plugin.tag");
    assert_eq!(last_tag, "main");
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
    let mtime_before = std::fs::metadata(&head_path).expect("stat head").modified().expect("mtime");

    // Sleep a beat so a second clone would produce a distinguishable mtime.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let _: bool = lua.load(&install_script).eval().expect("install 2");
    let mtime_after = std::fs::metadata(&head_path).expect("stat head 2").modified().expect("mtime");
    assert_eq!(mtime_before, mtime_after,
        ".git/HEAD mtime must not change on idempotent re-install");
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
    assert!(msg.contains("nefor-pm[broken]"), "error must be labelled by name: {msg}");
    assert!(msg.contains("git exited"), "error must surface the structured exit: {msg}");
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
    std::fs::write(
        plug_dir.join("subm.lua"),
        "return { tag = 'subm' }\n",
    )
    .expect("write subm");
    std::fs::write(
        plug_dir.join("init.lua"),
        "return { tag = 'root' }\n",
    )
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

