# Verification lanes and prepared execution

`tools/test-lanes.json` and Cargo target metadata jointly own deterministic test
membership. Ordinary targets are in the default lane; targets requiring their
package's `full-tests` feature join the full lane. The lane checker rejects a
target that cannot be classified from those sources.

The default and full Rust lanes use one prepared-artifact boundary:

1. Cargo metadata identifies workspace targets, package working directories,
   and lane requirements.
2. One Cargo producer builds the selected ordinary test executables and every
   workspace runtime binary, emitting machine-readable artifact paths.
3. Stable doctests run through a separately labelled Cargo/rustdoc exception.
   They run before signing because stable rustdoc cannot persist its executable
   artifacts for later direct execution.
4. The harness proves the producer inventory against metadata, writes a
   read-only manifest, and signs and strictly verifies every manifest executable
   on macOS. Signing is a portable no-op on other platforms.
5. The harness executes the exact manifest test paths from their package working
   directories. Each goes through the repository watchdog, which streams and
   captures output, enforces the remaining lane deadline, and terminates process
   descendants. Full-lane serialized-package arguments come from the same lane
   registry. Failures are aggregated instead of stopping later executables.
6. Every signed path is strictly verified again after ordinary execution.

After signing starts, the harness itself starts only the prepared watchdog and
test executables plus signature verification. Full-lane operational checks that
consume runtime binaries reuse the prepared target directory and a pre-signing
Cargo metadata snapshot; they must not add a Cargo freshness pass.

Each run preserves its immutable manifest, Cargo JSON, phase logs, signing list,
and watchdog output beneath `tmp/prepared-tests/`. These artifacts are the
evidence boundary for inventory, phase timing, process kind, failures, and
timeouts.

## Compaction consumer and process coverage

`compaction_bridge` is a default target. Its consumer regression parses the
root/profile snapshot shape through both Lua and Rust, compiles configuration
agents against the current public MAG modules, and constructs the real lazy LLM
factories with the asynchronous broker. `NEFOR_SOURCE_CONFIG` selects a source
configuration directory for paired acceptance; without it, the target uses canonical
profile nodes and a deterministic policy snapshot. Paired acceptance reads
only model-profile and MAG sources, not provider authentication, and neither
installs nor modifies configuration.

`compaction_process` is full-only. It runs separate engine, MAG, tool-gate and
native-mock processes with the shipped canonical manager and provider compositor.
Its retained traces cover delayed/out-of-order write receipts, notice delivery,
coalesced selves and gated siblings, hard/Disabled/unsupported/missing-capacity
outcomes, successful native evidence without later usage, structured and dynamic
workers, cancellation with late native results, and effects-free replay. These
process witnesses complement the focused boundary/manager tests; an isolated
synchronous facet fixture is not evidence for that transport claim.

## Explicit local-package process fixtures

The CLI mock process tests copy the starter configuration into retained project
`tmp/` directories and replace only the external classifier package with a local
test fixture. Providers, gate, tools, MAG, sessions and CLI still use their real
routed owners. Fixture observation does not perform settlement.

Before starting the TUI scenario clocks, the recipe installs the starter's
external packages once into a private suite data directory through `nefor-pm`.
`examples/nefor-agent/config/packages.lua` owns the same release pins and
verification for both preparation and normal startup. Each scenario gets a copy
of the prepared package tree and lockfile, while its sessions and other mutable
data remain isolated. Startup still resolves and verifies those packages; UI
readiness assertions do not include public-network transfer time. Preparation
requires network access unless an explicit local executable is selected.

The TUI scenario recipe accepts the optional `NEFOR_TEST_DA_BINARY` environment
variable for offline execution with an existing real classifier executable.
`tools/stage-tui-local-package.ts` stages an isolated config/package and adjusts
only copied scenario launch arguments. The executable is not a classifier stub;
permission-policy scenarios continue exercising its actual behavior. Without
this explicit test-only option, normal package resolution is unchanged. Neither
fixture edits the production starter or its archive pin.
