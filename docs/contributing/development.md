# Development guide

This section is for people changing Nefor itself. User installation and configuration belong in the root README and the component READMEs; do not put maintainer-only procedures there.

## Before changing code

Read `CLAUDE.md` for the repository rules and `docs/manifesto.md` for the product constraints. Then use `just --list` as the command index. The recipes are the supported developer interface; documentation should explain when to use them, not duplicate their implementation.

The workspace uses the stable Rust toolchain selected by `rust-toolchain.toml`, Cargo, Lua 5.4 embedded through `mlua`, and shell tooling used by a few recipes. `just setup` fetches dependencies. `just run` builds and launches the starter against the exact checkout, marks dirty development provenance, and sets all development paths explicitly.

## Change loop

1. Locate the owning layer in [architecture.md](architecture.md). Keep mechanism in the engine/plugins/shared libraries and policy in composition.
2. Add the narrowest meaningful test at the owning boundary. Prefer pure Rust or Lua tests; add a process or TUI scenario only when the behavior crosses that boundary.
3. Implement the complete change without retaining pre-public compatibility shims across minor releases.
4. Run the appropriate confidence tier from [testing.md](testing.md).
5. Run `just fmt` when Markdown or Rust formatting changed, then inspect the diff. `just fmt` includes Prettier for every Markdown file, so check that unrelated prose was not rewritten before staging.
6. Before committing, run at least `just check`. Run `just lint` whenever Rust changed. Cross-cutting and release-sensitive changes require the stronger suites described in the testing guide.

Do not invoke underlying Cargo commands when a matching recipe exists. Focused Cargo runs are reasonable while diagnosing a failure, but completion evidence should name the canonical recipe or CI-equivalent suite.

## Git behavior

History is linear: rebase feature branches and fast-forward them; do not create merge commits. Inspect `git log --oneline -10` before the first commit and use the existing concise, imperative message style.

The repository hooks are intentionally asymmetric:

- `.githooks/pre-commit` checks Rust formatting only.
- `.githooks/pre-push` runs Clippy and `just test`, then may create and push the release tag derived from the committed workspace version. Read [release.md](release.md) before pushing a version bump.

Hooks are not the complete local confidence story: Markdown formatting, release bundles, deterministic TUI scenarios, and the full workspace suite are explicit recipes.

## Common change boundaries

- Engine lifecycle, paths, session-log substrate: `engine/`.
- Shared wire types and plugin helpers: `crates/nefor-protocol`, `crates/nefor-plugin-sdk`, and `crates/nefor-sse`.
- MAG language/compiler: `crates/nefor-mag`; MAG runtime bridge/kernel: `plugins/mag`.
- Process-isolated capabilities: `plugins/*`.
- Bus-aware reusable behavior: `lua/core` and `lua/libs`.
- Shipped policy and composition: `starter/`; headless composition: `cli-config/`.
- Release/install assembly: `tools/bundle-release.sh`, `tools/plugin-binaries.sh`, `justfile`, and `.github/workflows/release.yml`.

See [documentation.md](documentation.md) when a change affects docs, examples, commands, wire behavior, or architecture.
