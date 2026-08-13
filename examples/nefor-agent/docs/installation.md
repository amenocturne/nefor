# Installation

Nefor installs two things: runtime binaries and a user-owned Lua composition.
Installing or upgrading the runtime does not imply replacing the config.

## Compatibility

Published release artifacts currently cover:

| Platform             | Target                      |
| -------------------- | --------------------------- |
| macOS, Apple silicon | `aarch64-apple-darwin`      |
| Linux, x86-64        | `x86_64-unknown-linux-gnu`  |
| Linux, ARM64         | `aarch64-unknown-linux-gnu` |

Building from source may work on other Rust-supported systems, but those systems
are not exercised by the release artifact matrix. The starter and source
installer expect a Unix-like environment and Git. Building requires the stable
Rust toolchain and Cargo; repository recipes require `just`. The starter's
command classifier, `da`, is installed through Homebrew when available or
through Cargo otherwise.

The repository includes a Nix flake and Home Manager module, but this route is
**experimental**: it is not part of the release artifact matrix or the primary
installation checks.

## Source checkout

This is the simplest complete install and the credential-free quick start:

```sh
git clone https://github.com/amenocturne/nefor.git
cd nefor
just install
```

The default `source` channel compiles the current checkout. You can select an
artifact channel while still using the checkout's installer:

```sh
just install latest   # Homebrew when available, otherwise latest stable tarball
just install nightly  # rolling nightly tarball
```

The second installer argument controls starter replacement:

```sh
just install source safe   # default: preserve an existing config
just install source force  # delete and recopy ~/.config/nefor
```

`force` is destructive. Back up or version-control your config first.

If `${PREFIX:-$HOME/.local}/bin` is not already on `PATH`, add it in your shell
configuration:

```sh
export PATH="${PREFIX:-$HOME/.local}/bin:$PATH"
```

## Homebrew

Install the stable engine and bundled plugins:

```sh
brew install amenocturne/tap/nefor
```

Homebrew ships the starter as shared data but does not create your user config.
Scaffold it explicitly:

```sh
mkdir -p ~/.config/nefor
cp -R "$(brew --prefix)/share/nefor/examples/nefor-agent/." ~/.config/nefor/
```

Do not copy over an edited config unless replacement is intentional.

## Start

Launch from the working directory tools should inherit:

```sh
nefor
```

The installed starter defaults to `mock-plugin` / `mock-model`, so first launch
needs no credentials. `nefor run` accepts starter-owned arguments such as
`--session`, `--prompt`, and `--mode`; engine path overrides remain global:

```sh
nefor --config /path/to/config --data-dir /path/to/data \
  run --mode safe
```

## Paths and precedence

| Purpose                      | Override                                                               | Default                                           |
| ---------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------- |
| Config containing `init.lua` | `--config`, then `NEFOR_CONFIG_DIR`                                    | `$XDG_CONFIG_HOME/nefor`, or `~/.config/nefor`    |
| Writable runtime data        | `--data-dir`, then `NEFOR_DATA_DIR`                                    | `$XDG_DATA_HOME/nefor`, or `~/.local/share/nefor` |
| Sessions                     | `NEFOR_SESSIONS_DIR`                                                   | `<data-dir>/sessions`                             |
| Plugin binaries              | Lua distribution helper (`NEFOR_EXECUTABLE_ROOT` override)                   | source installs use `~/.local/share/nefor/bin`    |\n| Aggregate log file           | `NEFOR_LOG_STDERR`; otherwise `--log-file`, then `NEFOR_LOG_FILE`             | `<data-dir>/logs/nefor.log`                       |\n\n`NEFOR_LOG_STDERR` takes precedence over file paths when set to a non-empty value. Explicit file paths are used as selected, including relative paths; the engine creates their parent directories and does not relocate them under the data directory.

`NEFOR_DEV_DIR` is a deliberate live-checkout override used by `just run`, not a
normal installed-user setting. Installed distributions may set
`NEFOR_RUNTIME_ROOT` to their immutable runtime checkout.

The engine writes `nefor.log` in the resolved config directory. Provider data,
including ChatGPT OAuth state when that provider is used, belongs under the
resolved data directory.

## Updating

- **Homebrew:** `brew upgrade nefor` updates the runtime. It does not replace
  `~/.config/nefor`.
- **Source:** update the checkout, then run `just install-nefor source`. This
  updates binaries only.
- **Stable/nightly artifact channel:** rerun `just install-nefor latest` or
  `just install-nefor nightly` from a checkout containing the installer.

Starter updates are intentionally separate. Compare your config with the new
checkout's `examples/nefor-agent/`, or use `just install-example force` only when deleting
and recopying the whole config is acceptable.

Homebrew installations can be removed with `brew uninstall nefor`. The source
installer has no uninstall recipe; its printed install paths are the authority
for manually managed files.
