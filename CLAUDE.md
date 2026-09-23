# nefor — architecture map

## What this is

Agent harness substrate. Pure string-bus engine + separate-process plugins (NCP over JSON-line stdio) + Lua composition. Plugins can be Rust or any language that can produce JSON lines on stdout and consume them on stdin. Lua stays embedded for `init.lua` composition; the rest is process-isolated.

## Layout

- `engine/` — engine binary. Reads plugin stdin, stamps `{origin, ts}`, appends to its in-memory event log, invokes a required Lua `dispatch` hook, routes the hook's `nefor.engine.send` calls. All NCP semantics live in Lua.
- `crates/nefor-protocol/` — NCP envelope + system-body types. Used by plugins; engine no longer imports it (engine is pure string-bus).
- `plugins/nefor-tui/` — declarative TUI plugin (Rust): reconciler + line-diff renderer + Lua VM + 15 layout primitives. Hosts the chat surface as a Lua composition (`examples/nefor-agent/chat/init.lua`).
- `plugins/generic-provider/`, `plugins/generic-tool/` — passive type-registry hubs owning canonical types (`ProviderRequest`, `ProviderInput`, `ChatHistory`, `ToolCalls`, `ToolResults`, …). Concrete providers/tools declare `Into`/`From` against these so graphs are provider-agnostic.
- `plugins/openai-provider/` — generic OpenAI-compatible provider with chat-id-keyed `Chats` map (`<prefix>.chat.{create, append, complete, delete}`). Configurable base URL + model. Declares `Into` against `generic-provider` types.
- `plugins/tool-gate/` — tool advertisement aggregator + permission gate. Sources advertise via `tools.advertise`; callers invoke via `tool.invoke`; gate forwards as `<source>.tool.invoke` and echoes `tool.result`.
- `plugins/mag/` — MAG runtime: compiles `.mag` source into immutable versioned inline program/delta envelopes and executes program constellations or applies deltas — the only execution path; it retains no program cache or callable compiler environment. See its `docs/`. Ships its own kernel Lua tree at `plugins/mag/lua/mag-kernel/` (actor fold, factories, routing, run contexts, observer stream), loaded by the plugin's embedded VM and resolved off `--lua-root`'s parent; `--kernel <path>` overrides it.
- `plugins/basic-tools/` — file/image reads, file writes, structured process execution, and explicit POSIX shell scripts.
- `plugins/git-worktree/` — stateless Git worktree capability provider. `git_worktree_create` is fresh-only; `git_worktree_open` validates explicit reuse. MAG wraps both as typed graph nodes and never removes successful worktrees.
- `plugins/mock-plugin/` — scriptable NCP actor for deterministic integration tests. Repository-owned tests never invoke live providers.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `examples/nefor-agent/init.lua` — default composition. Sets `package.path`, bootstraps the shared Lua tree via `nefor-pm`, defines the global `dispatch` hook (delegates to `core.ncp.dispatch`), and spawns every actor via `actor.spawn` (sessions, agentic-loop, providers, mag kernel, tool-gate, lead-workflow, chat).
- `lua/core/` — shipped library: NCP (handshake, broadcast-minus-sender, replay-on-attach, errors), actor runtime, history replay. JSON via the engine-provided `nefor.json`.
- `lua/libs/conversation-manager/` — canonical conversation authority: validates and sequences recorded semantic facts, owns provider-neutral conversation state and projections, and supplies model context reconstructed from those facts. Replay rebuilds this state without re-executing turns.
- `lua/libs/agentic-loop/` — transient lead-turn orchestration (`libs.agentic-loop`): queues user input, clones the config's turn-program (`examples/nefor-agent/agentic-loop/lead-turn.mag`), submits it to the MAG kernel, and coordinates ephemeral provider chats for the active turn. Native `/compact` asks conversation-manager for the canonical context, persists the compatible opaque artifact at a transcript cutoff, and restores it into later ephemeral MAG chats; the recorded full conversation remains the fallback. `examples/nefor-agent/agentic-loop/` keeps the config-owned turn-program data (`lead-turn.mag`) plus a re-export shim.
- `mag/` — the immutable `nefor-mag` package: canonical core/Nefor MAG modules, the MAG Book, and its compilable examples. The starter registers it through `nefor-pm`; configuration explicitly chooses its module root and guides.
- `lua/libs/mag-workspace/` — MAG workspace management **mechanism** (`libs.mag-workspace`): creates per-session writable source with an optional local `lib/` and formats graph previews. It never copies canonical packages or docs. Config-owned JSON manifests and prompt fragments remain at `examples/nefor-agent/mag/lib/` and are selected as a separate module/file-input root.
- `lua/libs/mag-context/` — ordered ambient-context assembly shared by the lead and every graph LLM/structured-output actor: authored system first, configured quick guides, full-book path and module inventory, then other ambient sections.
- `lua/libs/sessions/` — sessions actor **mechanism** (`libs.sessions`): composition-configured root, boot/shutdown/resume + jsonl persistence over the bus. The Rust engine exposes only generic filesystem primitives and has no session API or root selection. The test-only escape-hatch surface stays at `tests/lua/sessions/test.lua` and is loaded explicitly by the session test harness.
- `examples/nefor-agent/chat/` — the chat surface's **opinion layer**: `init.lua` (composition root — installs the require searchers and hands view/update to `tui.start`), `statusline.lua` (segments), `slash.lua` (command registry), `commands.lua` (starter slash/input policy). These `require("libs.chat.<m>")` for the mechanism. The state→view and event/key reducer mechanisms live in `lua/libs/chat/` (see below).
- `lua/libs/chat/` — chat **mechanism** (config-agnostic render/state): `controller.lua` owns deterministic event/key reduction over replaceable named handler groups; `dispatch.lua` combines those groups with explicit duplicate-registration policy. The example contributes command handling as a leaf group and may replace individual lifecycle stages. `entry.lua` (copy-on-write entry model with a global version counter), `entries.lua`, `view.lua` (top-level layout builder — receives the config-owned `statusline`/`slash` render opinions explicitly), `transcript.lua`, `queued_input.lua` (single owner for optimistic queue entries, durable echo reconciliation, steering acceptance, and hard-stop restoration), `workflow_controls.lua` (pure Esc/x/X decisions), `model_selection.lua` (single primitive behind the `/model` picker and every `/model` argument form: pending-pair correlation, atomic provider/model/reasoning-default/context adoption, rollback on rejection), `popups.lua`, `run_panel.lua`, `agent_streams.lua`, `height_cache.lua` (heights cached by `(version, width)`), `history.lua`, `common.lua`, `at_path.lua`, `log.lua` (debug logging gated on `NEFOR_CHAT_DEBUG`), `sessions.lua`. Downstream configs `require` these instead of copying. Virtual scroll uses gap=0 outer column with spacers flush against a nested content column to avoid phantom-gap position mismatches.
- `lua/libs/cli/` — noninteractive frontend (`libs.cli.start`): submits canonical `chat.input.submit`, waits for correlated whole-request completion and session flush, and prints final text/JSON. Selected by the starter's `run --frontend cli --prompt ...`; no TUI, stdin REPL, or separate agent loop. The development virtual `agentic-cli` entry delegates to this same frontend.
- `lua/libs/startup/` — shared agent-distribution startup parser and explicit mode application. Frontend selection stays in Lua composition, not engine source.
- `lua/libs/lead-workflow/` — lead-workflow **mechanism**: the `mag-write-file` / `mag-preview` / `mag-apply` / `write-review` / `mag-status` / `mag-await` / `mag-terminate` tool surface (`init.lua`) and persona-prompt loader (`role.lua`). Config-agnostic — resolves prompt/data roots from `NEFOR_CONFIG_DIR`/`NEFOR_DATA_DIR`, never from file location, so the persona prompt stays config-owned at `<config>/prompts/lead.md`. `examples/nefor-agent/lead-workflow/` holds thin re-export shims (`init.lua`, `role.lua`) so `examples/nefor-agent/init.lua`'s `require("lead-workflow")` spawn site resolves unchanged; downstream configs `require("libs.lead-workflow")` directly.
- `lua/libs/compositors/` — actor-spec builders per plugin binary (`libs.compositors.{provider,tools,chat_bridge}`): pure mechanism the config composition consumes to spawn provider/tools/chat-bridge actors. Resolves binary paths through the config's `config.bin(...)`, an implicit config interface.
- `lua/libs/read-only-tools/` — config-selected repository instruction discovery and ordinary skill loading, with tool-gate advertisement/dispatch. `build{ include, extra_tools = { { schema, handler } } }` returns an actor spec; registered handlers use `function(args, emit)` and settle through `emit.ok(text)` / `emit.err(msg)`. The starter enables `discover_instruction_files`; downstream configs select `skill` or register their own typed tools through the same seam.
- `lua/libs/tool-validator/` — tool-permission validator **mechanism** (`libs.tool-validator`): classifies gated invocations (shell.script through `da`, structural process.exec policy, edit/write policy, read-only auto-approve) into approve/deny/popup. `build{ auto_approve_tools, shell_fastpaths, process_fastpaths }` returns the actor spec — `auto_approve_tools` names tools approved unconditionally, `shell_fastpaths` inspects shell text before `da`; `process_fastpaths` inspects argv structurally without joining it. Config-owned policy plugs in through those seams. `examples/nefor-agent/tool-validator/` is a thin composition file that builds the base policy with no extras.
- `examples/nefor-agent/mock-provider/` — script loaded by `mock-plugin` to impersonate an openai-compatible provider with deterministic responses.
- `examples/nefor-agent/config/` — settings table consumed by `examples/nefor-agent/init.lua`.

## Path resolution

`nefor` resolves config and writable data directories via XDG-style env vars, with CLI flags taking highest precedence. Plugin commands and immutable runtime source are selected by Lua/distribution helpers:

| Env / launcher input    | CLI flag     | Default                          | Owner / purpose                                                                                        |
| ----------------------- | ------------ | -------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `NEFOR_CONFIG_DIR`      | `--config`   | `$XDG_CONFIG_HOME/nefor`         | Engine-selected directory containing `init.lua`                                                        |
| `NEFOR_DATA_DIR`        | `--data-dir` | `$XDG_DATA_HOME/nefor`           | Engine-selected writable data root                                                                     |
| `NEFOR_LOG_FILE`        | `--log-file` | `$NEFOR_DATA_DIR/logs/nefor.log` | Engine aggregate log destination; non-empty `NEFOR_LOG_STDERR` takes precedence over every file choice |
| `NEFOR_SESSIONS_DIR`    | —            | `$NEFOR_DATA_DIR/sessions`       | Starter composition-selected session event-log and MAG root                                            |
| `NEFOR_DEV_DIR`         | —            | (unset)                          | Starter distribution's live-checkout override                                                          |
| `NEFOR_RUNTIME_ROOT`    | —            | distribution-managed             | Starter distribution's immutable Lua/runtime source root                                               |
| `NEFOR_EXECUTABLE_ROOT` | —            | distribution-managed             | Starter distribution's plugin-command root                                                             |

The engine executes the exact commands registered by Lua. It has no plugin directory, discovery, manifest, or installation-provenance model; those are distribution concerns.

If no `init.lua` is found, the engine prints a friendly error pointing at the README install section.

## Manifesto

[`docs/manifesto.md`](docs/manifesto.md) governs Nefor development. Check every feature design against it before changing code. If a feature conflicts with the manifesto, redesign or reject the feature; only rarely, when the conflict exposes a fundamental mistake, reconsider the manifesto explicitly before making code changes.

## Conventions (enforced)

- Errors: `thiserror` for domain errors, `anyhow` only at the top boundary (`main.rs`).
- No `unwrap()` / `expect()` outside tests.
- Newtype every domain ID (`PluginId`, `SessionId`, `RunId`, `NodeId`, `FiringId`, `ChatId`, `ConfigDir`, `DataDir`).
- Enums (ADTs) for state; no boolean flags alongside sentinel variants.
- Immutability by default; I/O only at boundaries.
- No YAML/TOML/JSON config schema in core — config is `init.lua`.
- Plugins are separate OS processes communicating via NCP (see `docs/protocol.md`).
- Comments only for non-obvious _why_; code is self-documenting for _what_.

## Commands

- Verification breadth is part of this repository's workflow, not a generic completion ritual. During development, run only tests targeted at the code and behavior that changed.
- For an ordinary scoped code commit, run the relevant targeted tests and then `just check` once. This is the repository-defined completion lane; do not add broader suites merely because the work is being committed. Documentation-only changes need only their relevant documentation checks.
- Run subsystem recipes such as `just test-example`, `just test-tui-chat`, or targeted package Clippy only when the affected behavior requires them. Run `just test-integration` only for changes crossing the process-level integration boundaries it covers. Reserve `just test-all` and workspace-wide `just lint` for changes with an actual cross-workspace blast radius, release validation, or an explicit assignment naming that breadth. Reaching the end of a work unit, verification phase, or commit is not itself a trigger for any broader check. Do not run multiple broad recipes as cumulative reassurance.
- `just run` — launch engine with `./examples/nefor-agent` config (debug build). Sets `NEFOR_DEV_DIR` so Lua files load from the repo, not the installed copy.
- `just check` — formatting, documentation, the focused fast confidence set, and verification-lane completeness; the ordinary scoped pre-commit check.
- `just test` / `just test-default` — every bounded default deterministic target, including the registered non-Cargo default checks. Bare workspace Cargo exposes the same Rust target membership.
- `just test-full` / `just test-all` — every deterministic default and full target plus the registered non-Cargo full checks. Full Cargo targets require their package's `full-tests` feature and remain absent from bare Cargo. Rust targets and runtime helpers are produced once, captured in an immutable manifest, signed on macOS, and executed directly through the watchdog; stable doctests are the explicit conventional Cargo/rustdoc exception before signing. See `docs/testing.md`.
- `just lint` — workspace-wide Clippy with `-D warnings`; use targeted package Clippy for scoped Rust changes.
- `just fmt` — rustfmt.
- `just build` — release build into `target/release/`.

## Protocol docs

- Current NCP behavior: `docs/protocol.md`.
- Architecture/writing principles: `docs/principles.md`.
- Execution layers (engine / plugins / Lua trait layer / MAG): `docs/architecture.md`.

## Architecture

[`docs/architecture.md`](docs/architecture.md) is the canonical current architecture and ownership guide, including the engine, plugins, Lua composition and libraries, and MAG layers. Keep placement guidance there rather than duplicating it here.

## Compatibility policy (pre-public)

Compatibility guarantees apply to published releases: `0.y.x` stays backwards compatible with `0.y.0`, while a new minor release may break compatibility. Prefer the clean shape over migration paths, compat shims, or old-session support: do not build fallbacks for prior wire formats, session layouts, or config shapes; delete replaced code instead of deprecating it. Old sessions failing to resume across a minor bump is acceptable. This holds until the project goes public and gains daily-driver users.

## Versioning

Keep the workspace version in `Cargo.toml` at the last published release, currently `0.4.0`, while developing. Identify unreleased work by its Git commit; do not assign the next release version to work in progress.

Change the version or create and publish a stable release tag only when the user explicitly requests a release. Ordinary commits and pushes do not create releases. For a requested release, select the version relative to the last published release: breaking changes require a new minor version; compatible changes use a patch version. Release tags use `v0.x.y`.

## Git

- **Rebase, not merge.** Always rebase feature branches onto main before fast-forwarding. No merge commits in the history.
- Check `git log --oneline -10` before your first commit to match existing message style.
- Minimal one-line commit messages — no body unless the "why" isn't obvious from the diff.
- No Co-Authored-By lines, no emoji prefixes, no conventional-commit prefixes.
