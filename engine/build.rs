//! Embed a richer build version into the binary so `nefor --version`
//! reports e.g. `0.1.5` for a tagged build, `0.1.5-12-gabcdef` for a
//! nightly between tags, or `0.1.5-12-gabcdef-dirty` for a build with
//! uncommitted changes.
//!
//! Falls back to `CARGO_PKG_VERSION` when `git describe` fails (no git
//! on the build machine, no tags reachable, building from a tarball
//! without a `.git` directory). The fallback isn't a regression — it
//! matches the prior `env!("CARGO_PKG_VERSION")` behaviour.

use std::process::Command;

fn main() {
    // Linked worktrees split per-worktree HEAD/index state from common refs.
    // Resolve every path through Git and watch only the checked-out branch,
    // rather than every branch in every worktree.
    for path in ["HEAD", "logs/HEAD", "packed-refs", "refs/tags"] {
        emit_git_watch(path);
    }
    if let Some(symbolic_head) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        emit_git_watch(&symbolic_head);
    }
    println!("cargo:rerun-if-env-changed=NEFOR_VERSION_OVERRIDE");

    let version = if let Ok(v) = std::env::var("NEFOR_VERSION_OVERRIDE") {
        // Workflow escape hatch: lets a release job stamp an exact
        // version string without relying on git state in the runner.
        v
    } else {
        git_describe().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
    };

    println!("cargo:rustc-env=NEFOR_VERSION={version}");
}

fn emit_git_watch(path: &str) {
    if let Some(path) = git_path(path).filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_path(path: &str) -> Option<std::path::PathBuf> {
    git_output(&["rev-parse", "--path-format=absolute", "--git-path", path]).map(Into::into)
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        // Read-only Git queries must not refresh the watched index and make
        // the build script invalidate itself on the next Cargo invocation.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let value = raw.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Run `git describe --tags --always --dirty --match v*` and return the
/// stripped output. The `v` prefix on tags (matching the release tag
/// shape `vX.Y.Z`) is dropped so `nefor --version` reports a
/// SemVer-shaped string. Returns `None` on any failure.
fn git_describe() -> Option<String> {
    let description = git_output(&[
        "describe",
        "--tags",
        "--always",
        "--dirty",
        "--match",
        "v[0-9]*.[0-9]*.[0-9]*",
    ])?;
    Some(
        description
            .strip_prefix('v')
            .unwrap_or(&description)
            .to_owned(),
    )
}
