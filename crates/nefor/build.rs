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
    // Re-run when HEAD moves or refs change. `.git/HEAD`'s file content
    // only changes on branch switch (it's a symbolic ref like
    // `ref: refs/heads/foo`), so watching it alone misses new commits on
    // the current branch. `.git/logs/HEAD` records every HEAD movement
    // (commit, reset, checkout, merge) — that's the right signal. We
    // also watch the refs dirs so tag creates/moves trigger a rebuild.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/tags");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
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

/// Run `git describe --tags --always --dirty --match v*` and return the
/// stripped output. The `v` prefix on tags (matching the release tag
/// shape `vX.Y.Z`) is dropped so `nefor --version` reports a
/// SemVer-shaped string. Returns `None` on any failure.
fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--always",
            "--dirty",
            "--match",
            "v[0-9]*.[0-9]*.[0-9]*",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip leading `v` so `v0.1.5-12-gabcdef` becomes `0.1.5-12-gabcdef`.
    Some(trimmed.strip_prefix('v').unwrap_or(trimmed).to_owned())
}
