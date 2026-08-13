# Distribution and replacement

The `nefor` engine is deliberately voiceless. A usable distribution supplies Lua runtime modules, an `init.lua` composition, subprocess binaries, provider/tool policy, defaults, and usually an interface. The shipped starter is one such distribution.

## Supported levels

- **Customize starter values:** edit a copied starter configuration.
- **Select another config:** pass `--config` or `NEFOR_CONFIG_DIR`.
- **Replace composition:** supply another `init.lua`, while reusing selected runtime libraries.
- **Replace the distribution:** own the runtime source generation, binaries, composition, install/update path, and compatibility policy.

The first two preserve starter assumptions. Replacing `init.lua` is intentionally powerful but tightly coupled: the replacement must wire actors, NCP wrappers, sessions, readiness, providers, tools, MAG, and UI consistently. Internal MAG kernel factories and registries are not a supported shortcut for doing so.

## Runtime source selection

The current starter selects shared Lua source in this order:

1. a valid mutable checkout named by `NEFOR_DEV_DIR`;
2. a valid immutable generation named by `NEFOR_RUNTIME_ROOT`;
3. a version-derived managed checkout under the data root.

Use `NEFOR_DEV_DIR` only for live source development. Installed distributions should set `NEFOR_RUNTIME_ROOT` to immutable source matching their binaries and record an installation identity. The Home Manager module currently uses `NEFOR_DEV_DIR` for immutable Nix source; that is current packaging behavior, not the preferred external-distribution contract.

Config-local modules precede shared runtime modules on `package.path`. The starter then registers already-materialized module directories with `nefor-pm`; immutable distributions should do the same rather than creating mutable lock state.

The engine consumes none of these roots. It executes the explicit command arrays the composition registers and carries no plugin directory, discovery, manifest, or installation-provenance state. The distribution helper must therefore resolve every executable and runtime source before spawn.

## Installer channels

From a source checkout:

```sh
just install source safe
just install latest safe
just install nightly force
```

Channels are `source`, `latest`, and `nightly`. `latest` prefers Homebrew and otherwise downloads a stable release archive; `nightly` uses the rolling nightly release. Starter mode `safe` refuses to overwrite an existing config; `force` deletes and recopies it.

Source/archive installation exposes `nefor` and `mag` on `PATH`, installs runtime plugin binaries under the Nefor data tree, prunes obsolete managed binaries, and installs `da` separately. `just install-source` is the lower-level immutable-generation primitive and verifies that the resolved checkout has the expected commit before building it.

## Release bundle

A current release bundle has this shape:

```text
bin/nefor
bin/mag
share/nefor/plugins/<runtime binaries>
share/nefor/plugins.manifest
share/nefor/examples/nefor-agent/**
share/nefor/LICENSE
share/nefor/README.md
share/nefor/CHANGELOG.md       # when present
```

`tools/bundle-release.sh` constructs it and `tools/test-release-bundle.sh` is the executable packaging contract. Published archives currently target Apple Silicon macOS and x86_64/AArch64 GNU Linux. Stable tags and the rolling nightly channel are built by the release workflow.

Nix exposes `nefor`, `nefor-engine`, `nefor-starter`, and a Home Manager module across its declared systems. Homebrew is another install channel, not a different runtime architecture.

## Virtual CLIs

A distribution can register Lua `cli` entries and expose them as `nefor plugin <name> ...`. These boot the selected config, so they are part of that distribution. The repository's `agentic-cli` is registered only by `cli-config/` for development and tests; it is not included in the starter or release bundle. See [CLI reference](../../../docs/reference/cli.md).

## Compatibility

Before a public stability commitment, compatibility is guaranteed only within a minor `0.y.x` line. A new minor line may break configuration, protocol, session, or module contracts rather than carrying compatibility shims. A distribution that pins/repackages Nefor must choose and document its own upgrade policy.

For package resolution and immutable registration, see [`nefor-pm`[`nefor-pm`](../../../lua/nefor-pm/README.md).
