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
- `plugins/mag/` — MAG runtime: actor kernel executing compiled `.mag` programs as in-memory actor constellations — the only execution path. See its `docs/`. Ships its own kernel Lua tree at `plugins/mag/lua/mag-kernel/` (actor fold, factories, routing, run contexts, observer stream), loaded by the plugin's embedded VM and resolved off `--lua-root`'s parent; `--kernel <path>` overrides it.
- `plugins/basic-tools/` — `read_file` / `write_file` / `bash` built-ins.
- `plugins/git-worktree/` — stateless Git worktree capability provider. `git_worktree_create` is fresh-only; `git_worktree_open` validates explicit reuse. MAG wraps both as typed graph nodes and never removes successful worktrees.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests. Local Ollama works through `openai-provider` directly with `static_token = "ollama-local"`.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `examples/nefor-agent/init.lua` — default composition. Sets `package.path`, bootstraps the shared Lua tree via `nefor-pm`, defines the global `dispatch` hook (delegates to `core.ncp.dispatch`), and spawns every actor via `actor.spawn` (sessions, agentic-loop, providers, mag kernel, tool-gate, lead-workflow, chat).
- `lua/core/` — shipped library: NCP (handshake, broadcast-minus-sender, replay-on-attach, errors), actor runtime, history replay. JSON via the engine-provided `nefor.json`.
- `lua/libs/agentic-loop/` — the lead's turn spawner **mechanism** (`libs.agentic-loop`): per user message it clones the config's turn-program (`examples/nefor-agent/agentic-loop/lead-turn.mag`) and submits it to the mag kernel; owns canonical history + queueing. Native `/compact` materializes that history into a temporary provider chat, persists the opaque artifact at a transcript cutoff, and restores it into later ephemeral MAG chats; the full transcript remains the fallback across provider/model switches. `examples/nefor-agent/agentic-loop/` keeps the config-owned turn-program data (`lead-turn.mag`) plus a re-export shim.
- `lua/libs/mag-workspace/` — MAG workspace management **mechanism** (`libs.mag-workspace`): per-session workspace seeding plus modification preview formatting; compilation itself lives in the mag plugin. `examples/nefor-agent/mag/lib/` keeps the config-owned seed content (types, templates, tools, policies, patterns, prompts) that a workspace is seeded from.
- `lua/libs/sessions/` — sessions actor **mechanism** (`libs.sessions`): boot/shutdown/resume + jsonl persistence over the bus. The test-only escape-hatch surface stays at `tests/lua/sessions/test.lua` and is loaded explicitly by the session test harness.
- `examples/nefor-agent/chat/` — the chat surface's **opinion layer**: `init.lua` (composition root — installs the require searchers and hands view/update to `tui.start`), `statusline.lua` (segments), `slash.lua` (command registry), `update.lua` (reducer + event handlers). These `require("libs.chat.<m>")` for the mechanism. The pure state→view mechanism lives in `lua/libs/chat/` (see below).
- `lua/libs/chat/` — chat **mechanism** (config-agnostic render/state, no bus emission): `entry.lua` (copy-on-write entry model with a global version counter), `entries.lua`, `view.lua` (top-level layout — assembles the opinion `statusline`/`slash` by fixed module name, an implicit interface a later wave parameterizes), `transcript.lua`, `queued_input.lua` (single owner for optimistic queue entries, durable echo reconciliation, steering acceptance, and hard-stop restoration), `workflow_controls.lua` (pure Esc/x/X decisions), `popups.lua`, `run_panel.lua`, `agent_streams.lua`, `height_cache.lua` (heights cached by `(version, width)`), `history.lua`, `common.lua`, `at_path.lua`, `log.lua` (debug logging gated on `NEFOR_CHAT_DEBUG`), `sessions.lua`. Downstream configs `require` these instead of copying. Virtual scroll uses gap=0 outer column with spacers flush against a nested content column to avoid phantom-gap position mismatches.
- `lua/libs/cli/` — virtual `agentic-cli` plugin (`libs.cli`): surfaces the loop over stdin/stdout for `nefor plugin agentic-cli "<prompt>"`. Pure mechanism consumed by CLI-surface configs (`cli-config/`).
- `lua/libs/state-tracking/` — best-effort runtime-state observer actor (`libs.state-tracking`): folds chat/session bus traffic into a small runtime state and fans it out to clamor state publishing + desktop input-needed notifications.
- `lua/libs/lead-workflow/` — lead-workflow **mechanism**: the mag / mag-eval / write-review / graph-status / terminate-graph tool surface (`init.lua`), the one-off MAG-expression tool (`mag-eval.lua`), and the persona-prompt loader (`role.lua`). Config-agnostic — resolves prompt/data roots from `NEFOR_CONFIG_DIR`/`NEFOR_DATA_DIR`, never from file location, so the persona prompt stays config-owned at `<config>/prompts/lead.md`. `examples/nefor-agent/lead-workflow/` holds thin re-export shims (`init.lua`, `role.lua`) so `examples/nefor-agent/init.lua`'s `require("lead-workflow")` spawn site resolves unchanged; downstream configs `require("libs.lead-workflow")` directly.
- `lua/libs/compositors/` — actor-spec builders per plugin binary (`libs.compositors.{provider,tools,chat_bridge}`): pure mechanism the config composition consumes to spawn provider/tools/chat-bridge actors. Resolves binary paths through the config's `config.bin(...)`, an implicit config interface.
- `lua/libs/read-only-tools/` — read-only investigation tools **mechanism** (`libs.read-only-tools`): the `list_dir`/`search_text`/`python-read`/`instructions`/`discover_instruction_files` handlers plus the tool-gate advertise/dispatch plumbing. `build{ extra_tools = { { schema, handler } } }` returns the actor spec, where a config-registered handler is `function(args, emit)` with `emit.ok(text)`/`emit.err(msg)` — the registration seam that lets a downstream config add typed tools (e.g. `mirror-projects`, `skill`) without forking. `examples/nefor-agent/read-only-tools/` is a thin composition file that builds the base set with no extras.
- `lua/libs/tool-validator/` — tool-permission validator **mechanism** (`libs.tool-validator`): classifies gated invocations (shell.script through `da`, structural process.exec policy, edit/write policy, read-only auto-approve) into approve/deny/popup. `build{ auto_approve_tools, shell_fastpaths, process_fastpaths }` returns the actor spec — `auto_approve_tools` names tools approved unconditionally, `shell_fastpaths` inspects shell text before `da`; `process_fastpaths` inspects argv structurally without joining it. Config-owned policy plugs in through those seams. `examples/nefor-agent/tool-validator/` is a thin composition file that builds the base policy with no extras.
- `examples/nefor-agent/mock-provider/` — script loaded by `mock-plugin` to impersonate an openai-compatible provider with deterministic responses.
- `examples/nefor-agent/config/` — settings table consumed by `examples/nefor-agent/init.lua`.

## Path resolution

`nefor` resolves directories via XDG-style env vars, with CLI flags taking highest precedence:

| Env var              | CLI flag       | Default                    | Holds                                                                                                        |
| -------------------- | -------------- | -------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `NEFOR_DEV_DIR`      | —              | (unset)                    | dev repo root — when set, Lua searchers resolve `plugins/*/lua/` and `examples/nefor-agent/` from here first |
| `NEFOR_LOCAL_DIR`    | —              | (unset)                    | installed-config local checkout override — lets pm use an unpushed local repo instead of fetching GitHub     |
| `NEFOR_CONFIG_DIR`   | `--config`     | `$XDG_CONFIG_HOME/nefor`   | `init.lua`                                                                                                   |
| `NEFOR_DATA_DIR`     | `--data-dir`   | `$XDG_DATA_HOME/nefor`     | writable runtime data other than sessions                                                                    |
| `NEFOR_SESSIONS_DIR` | —              | `$NEFOR_DATA_DIR/sessions` | session event logs, MAG trees, and provenance metadata                                                       |
| `NEFOR_PLUGIN_DIR`   | `--plugin-dir` | `$NEFOR_DATA_DIR/plugins`  | binaries                                                                                                     |

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

- `just run` — launch engine with `./examples/nefor-agent` config (debug build). Sets `NEFOR_DEV_DIR` so Lua files load from the repo, not the installed copy.
- `just test` — workspace tests.
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt.
- `just build` — release build into `target/release/`.

## Protocol docs

- Current NCP behavior: `docs/protocol.md`.
- Architecture/writing principles: `docs/principles.md`.
- Execution layers (engine / plugins / Lua trait layer / MAG): `docs/architecture.md`.

## Architecture

[`docs/architecture.md`](docs/architecture.md) is the canonical current architecture and ownership guide, including the engine, plugins, Lua composition and libraries, and MAG layers. Keep placement guidance there rather than duplicating it here.

## Compatibility policy (pre-public)

The only compatibility guarantee is within a single minor line: `0.y.x` stays backwards compatible with `0.y.0`. Across minor lines there are none — breaking changes ride the `0.y → 0.y+1` bump, and the flow is to mark a stable release as `0.y.0` and move on, breaking freely toward the next. In practice patch releases haven't occurred under this policy (existing `0.y.z>0` tags predate it). Prefer the clean shape over migration paths, compat shims, or old-session support: do not build fallbacks for prior wire formats, session layouts, or config shapes; delete replaced code instead of deprecating it. Old sessions failing to resume across a minor bump is acceptable. This holds until the project goes public and gains daily-driver users.

## Versioning

Workspace version is `0.x.y` in `Cargo.toml`. Users pin Lua libs to the engine's version tag via `nefor-pm`; breaking the API means their install breaks on next fetch.

- **Breaking changes bump `x`** (the minor in `0.x.y`): NCP wire protocol changes, Lua binding removals/renames, pm spec shape changes, example module interface changes that external configs depend on.
- **Non-breaking additions bump `y`**: new bindings, new pm features, new example modules, bug fixes.
- Tag format: `v0.x.y`. The release workflow and `nefor-pm` both key on this.

## Git

- **Rebase, not merge.** Always rebase feature branches onto main before fast-forwarding. No merge commits in the history.
- Check `git log --oneline -10` before your first commit to match existing message style.
- Minimal one-line commit messages — no body unless the "why" isn't obvious from the diff.
- No Co-Authored-By lines, no emoji prefixes, no conventional-commit prefixes.
