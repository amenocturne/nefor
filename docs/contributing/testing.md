# Testing guide

Nefor has confidence tiers rather than one command that is equally appropriate for every change. Choose the smallest tier that establishes the claim, then increase confidence when the change crosses process, UI, persistence, packaging, or release boundaries.

## Confidence tiers

| Tier          | Recipes                                                                                                                                                                                                                      | Use it for                                                                                                       |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Focused       | A specific test target while developing; named recipes such as `just test-pm`, `just test-provider`, `just test-starter`, `just test-tui`, `just test-tui-chat`, `just test-build-version`, or `just test-session-inspector` | One owned subsystem or a diagnosed regression.                                                                   |
| Default       | `just check` and, for Rust changes, `just lint`                                                                                                                                                                              | Ordinary localized changes. `check` is formatting verification plus the fast engine/starter/tool confidence set. |
| Cross-cutting | `just test-all`, plus the relevant focused suites                                                                                                                                                                            | Changes spanning crates/plugins, Lua composition, provider translation, sessions, or broad refactors.            |
| Process/UI    | `just test-integration` and, when useful, `just test-tui-all`                                                                                                                                                                | Runtime assembly, release contents, or behavior visible through the real starter TUI.                            |
| Release       | `just test-all`, `just lint`, `just test-release-bundle`, `just test-build-version`, and `just test-tui-scenarios`                                                                                                           | A release candidate or changes to versioning, installation, artifacts, runtime paths, or workflows.              |

`just test` aliases the fast default suite; it is deliberately not the entire workspace. `just test-all` follows CI's split for `nefor-tui`: workspace tests excluding the TUI package, TUI library tests, then serialized chat integration tests. Provider tests can require local socket binding. TUI scenarios require the environment-managed `tui-driver` binary and leave failure evidence in `tmp/tui-driver-artifacts/`.

## Where tests live

- `engine/src` unit tests cover engine-owned logic; `engine/tests` exercises Lua libraries, starter composition, sessions, conversations, package management, providers, and process-level CLI paths through the engine boundary.
- Each `plugins/*` package owns capability-specific Rust unit/integration tests. `plugins/mag/tests` covers the compiler/runtime contract, kernel invariants, shipped MAG corpus, and structured-provider flows.
- `plugins/nefor-tui/tests` owns chat-surface integration; its chat suite runs single-threaded because process environment and Lua composition are shared boundaries.
- `tests/tui/*.json` are deterministic end-to-end scenarios driven through the real engine and starter.
- `tools/test-release-bundle.sh` checks distribution assembly against the plugin binaries discovered from Cargo metadata.

Put a regression at the narrowest boundary that would have caught it. A test that exists only at a higher layer should be justified by behavior that genuinely emerges there.

## Manual and live checks

`just run` is the supported checkout-local TUI smoke path. It can contact whichever providers the starter enables, so it is not a hermetic test. Do not use personal credentials or live-provider behavior as the only evidence for a change that can be covered deterministically.

`just bench-mag` and `just bench-build` are opt-in diagnostics, not pass/fail gates. See `docs/mag-performance.md` for interpretation. The session inspector likewise has a focused backend recipe; opening its browser UI is a manual inspection step, not a substitute for tests.

## Reporting evidence

Report the exact recipes that passed and any suite not run. Do not infer broad health from a focused test. For TUI failures, preserve the generated artifact path. For environment-dependent failures (socket permissions, missing `tui-driver`, provider credentials), distinguish an unrun check from a product failure.
