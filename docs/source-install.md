# Source-install prerequisites

`just preflight-source-install` is the local, zero-argument prerequisite API.
It exits nonzero with the missing program, invalid override, wrong Lua version,
or unusable headers/library on stderr. It neither installs packages nor downloads
anything or changes shell/global LuaRocks settings. Compile/link probes use temporary
files under `tmp/` (or `SOURCE_PREREQUISITES_TMP`) and remove them afterwards.
Both source installers run it before preparing dependencies, building Rust, or
pruning existing installed binaries. Network failures can still occur afterwards.

## Installed prerequisites

Supported native hosts are Linux and macOS. Install these before source installation:

- Git, just, current Rust stable with Cargo; a C compiler (`cc`) and GNU make.
- LuaRocks 3.x, Lua **5.4** interpreter, development headers and library;
  pkg-config; OpenSSL **3** headers and ssl/crypto libraries.
- GNU m4 (cqueues generates `errno.c` from its included template), curl with a
  trusted CA certificate store, unzip, tar, gzip, and SHA256 via sha256sum or
  shasum. Bash and ordinary Unix file utilities are required.
- On macOS: otool, install_name_tool, codesign and realpath. Modified native
  modules and vendored non-system dylibs are signed by distribution preparation.

For Debian trixie the native packages are `build-essential m4 pkg-config lua5.4
liblua5.4-dev luarocks libssl-dev curl ca-certificates unzip git just`; install
current Rust stable separately (Debian's Rust may lag locked crate requirements).
On macOS install Homebrew `lua@5.4 luarocks openssl@3`, GNU m4 if absent,
and the compiler/relocation tools from Apple's command-line developer tools.
No shell prefix exports are needed. A default Lua 5.5 does not select the build ABI.

The locked aws-lc-sys 0.44.0 uses its C compiler builder on native Linux/macOS
with shipped bindings and default static crypto features. Its Cargo `cmake`
dependency alone does not require the CMake executable. Non-default AWS-LC
settings (FIPS, no-asm, dynamic libraries, bindgen, explicit CMake builder) or
cross-compilation may require additional tools; this native-host lane does not
validate those configurations. Build/test-lane tools (bun, ripgrep, managed
TUI driver, etc.) are separate; see [testing](testing.md).

## Discovery and overrides

Linux discovers Lua through `pkg-config lua5.4`, then checks versioned interpreter
and header locations, and uses the reported multiarch library directory.
OpenSSL uses pkg-config. macOS discovers installed Homebrew `lua@5.4` and
`openssl@3` prefixes even when keg-only Lua is absent from PATH.

`LUA_DEPENDENCIES_LUA_DIR` and `LUA_DEPENDENCIES_OPENSSL_DIR` explicitly select
prefixes. A provided empty, missing or wrong-version override fails; it never
falls back to another installation. A prefix must contain `bin`, `include`
(and Lua's versioned include subdirectory if applicable), and `lib`. Preflight
compiles, links and runs small Lua/OpenSSL probes to detect unusable libraries.
It prints the selected interpreter, headers and library directories.

Preparation uses a fresh private HOME, XDG config/cache and LuaRocks system/user
configuration, in an owned working directory; previously configured repositories,
Lua paths or caches are not required. Only local archive paths are passed to
LuaRocks with `--deps-mode=none`. The final empty-server check verifies that the
locked closure satisfies the distribution and every installed rockspec.
An error retains the old prepared tree; successful preparation atomically
replaces it and writes `dependencies.lock` and `installed-packages.tsv` provenance.
Linux modules use system OpenSSL; macOS carries relocated non-system dylibs.
See [distribution Lua dependencies](architecture.md#distribution-lua-dependencies)
for runtime certificate and platform-tree constraints.

## Declared network inputs

Each lock row requires `name version src-or-all sha256 https-url`. All ten
explicit archive URLs are on **luarocks.org**. Paths under `/manifests/<uploader>/`
are archive locations, not repository index requests. Preparation fetches only
these URLs, restricts redirects to HTTPS, preserves curl TLS/transport diagnostics,
and validates the entire downloaded closure before building. Every source rock
contains its source archive; installation does not resolve names against indexes.

A fresh complete source build additionally needs the locked Cargo crates
(**index.crates.io**, **static.crates.io**). Acquiring source uses **github.com**;
provisioning Rust uses **static.rust-lang.org** (and rustup distribution endpoints).
Those inputs are not covered by local preflight. Optional first-start package
manager downloads (for example da) and providers have independent network inputs.
Corporate egress/TLS policy can reject any of these; no certificate verification
is disabled and no undocumented mirror fallback is attempted.

## Isolated acceptance

`just accept-source-install REV /absolute/new/scratch` is opt-in and downloads
public artifacts/crates. It requires exact committed HEAD and clean tracked
source, declared installed prerequisites, and a previously nonexistent scratch
root. The installed-engine HTTP acceptance additionally requires uv and an installed Python 3 (its
Python test harness is managed through uv, not a source-build prerequisite;
acceptance disables managed-Python downloads).
Run it on a fresh Linux host/container without copied host Cargo/native
archives, caches or targets. It uses empty user/XDG directories, a fresh Cargo
cache and build target, no native prefix overrides, explicit installation
destinations, then the installed-engine `test-pm-http` acceptance (no provider).
For rustup, supply the installed toolchain's `RUSTUP_HOME` explicitly so changing
HOME does not hide the installed compiler; no Cargo cache is inherited.

The deterministic `just test-source-prerequisites` covers simulated discovery
and failure paths; simulated Darwin is not actual macOS installation evidence.
CI runs native discovery on macOS and fresh source-install acceptance on Linux.
