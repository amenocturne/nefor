# Changelog

## v0.5.0

- Delegated MAG runs now pin the lead's acknowledged provider/model selection and configuration-resolved named model profiles for their full lifetime, including dynamic actors and live additions. Typed profile-authored actors resolve against that immutable run snapshot, while ordinary actors preserve current-model behavior. Reasoning effort is optional end to end: explicit values remain provider-neutral, while omission stays omitted on the provider wire.
- Made the conversation manager the canonical owner of durable conversation facts, transcript projection, model context, and provider-scoped compaction checkpoints. `/compact` now works during live sessions, preserves the full transcript as fallback, carries the active model into native ChatGPT compaction, and reports structured provider failures intelligibly.
- Added a separately configurable shared session root, acknowledgement-first session replacement, and cooperative resume with progress. Semantic replay now reconstructs conversation state without re-running tools or workflows, rejects superseded replay traffic, preserves first-message listings, and no longer resurrects historical MAG notifications or approvals.
- Expanded MAG with typed products and sums, structured agent results, explicit Git worktree create/open nodes, detached execution, awaiting, run inspection, and scoped termination. New `process.exec` and POSIX `shell.script` primitives provide explicit timeouts plus structured stdout, stderr, exit, and signal results; `mag-eval` now resolves these libraries and preserves process results correctly.
- Reworked the TUI around run-scoped workflow ownership: concurrent runs have duration-first sidebar rows, bounded completed-run retention, foldable groups, chronological inspectors, and selected/all-run termination controls. Tool receipts, context usage, queued-input steering and recovery, safe Markdown links, initial CLI prompts, and path-aware compact rows are also reflected consistently.
- Extracted the chat reducer, lifecycle stages, transcript projection, queued-input ownership, run panels, and related rendering state into shared `lua/libs/chat` mechanisms with replaceable named handler groups. The bundled agent remains an explicit example composition, while downstream configs can replace individual stages without copying the whole chat surface.
- Added composition seams for providers, tools, read-only capabilities, validation policy, sessions, lead workflows, and CLI surfaces. The Rust engine is now fully session-blind, executes only explicit Lua-registered plugin commands, reports typed plugin-process termination facts without owning shutdown policy, and defaults aggregate logging to `<data-dir>/logs/nefor.log`; the starter/distribution owns session roots, executable/runtime resolution, offline closure, and the existing 2000 ms cascade shutdown.
- Hardened ChatGPT authentication and routing, model discovery, usage/quota refresh, compaction restore, structured output, tool calls, and image handling; fixed causal transcript and capability-preview duplication, queued-input reconciliation, overlapping MAG preview attribution, failed-run cleanup, and session-boundary UI reset.
- Approval mode now fails safe across startup, restart, and session resume: absent or stale durable authority resets to `safe`, replayed decisions do not reopen prompts, and explicit MAG human approvals remain separate from ordinary tool and write-review policy.
- **Breaking:** MAG sum-type constructors now live in the scope of their type and are written `Type.Constructor(...)` everywhere, for local, imported, aliased, and generic (`Option<Int>.Some(42)`) owners, in any expression position and without an annotation. The `named(Type, Constructor, payload)` form is removed, bare `Constructor(...)` no longer constructs, and a generic owner's type arguments are inferred from the payload and the expected type (`Option.Some(42)`, `let absent: Option<Int> = Option.None`) or written explicitly when neither determines them; the arguments of one call are solved together, so `same(Box.Empty, 1)` infers `Box<Int>`. An alternative without a payload is declared as a bare name (`type Timeout = Unlimited | Seconds(Int)`), constructed as `Timeout.Unlimited`, and matched as `case Unlimited =>`; the `C()` declaration form is removed, and type identities are unchanged. An inline record payload takes its fields by name or by position in declared order. A `let` annotation is now also the expected type for a generic call's result-only type parameters. Two types in a module may share a constructor name, and a constructor no longer shadows a function of the same name.
- MAG comma-separated forms (lists, tuples, call and constructor arguments, type and generic argument lists, import selectors, lambda parameters) may span lines, and a trailing comma is optional in all of them; previously a multi-line list without one failed with only `expected ','`. A missing comma between elements is reported at the end of the element it should follow.
- MAG imports now allow overloads: values imported under one name, and local `let` bindings of that name, collide only when two of them have the same type. One type imported under a name by several paths, directly and through transparent aliases, is one binding; different types under one name still collide.
- **Breaking:** the MAG context in agent prompts no longer lists every module under the module roots. A configuration now declares its public MAG modules through `libs.mag-context`'s `public_modules` (each embedded verbatim as source, or listed with its path only via `embed = false`) and its toolsets through `toolsets` (tool names rendered from the configuration's JSON manifest under the MAG name an author writes); only those are named in the prompt. Nefor's MAG library is split accordingly: `nefor.contracts`, `nefor.workflow`, `nefor.shell`, `nefor.process`, `nefor.path`, `nefor.worktree`, `nefor.collections`, `nefor.retry`, `nefor.human`, and `nefor.actors` hold the commented declarations authors write against, the `nefor` facade re-exports them, and wiring lives in private modules. Internal names moved: tool-call values and runtime records from `nefor.contracts` to `nefor.wire`, agent actor records from `nefor.actors` to `nefor.llm`, capability parameter records to `nefor.capability`; `RetryGateConfig`, `RetryDecision`, and `retry_gate` are now in `nefor.retry`, the toolset lists are `nefor.toolsets.general` / `read_only`, and the facade no longer exports `Indexed`. A generic `let` whose value is not a function literal is rejected with a message saying so. The starter's `agents` module keeps its model resolution in the internal `agents.models`.
- A model that calls a tool it cannot use is now told which tools it can: ``unknown tool `bash`; available tools: read_file, mag-apply``. The list is what that caller's request offered, in the same order, including a typed agent's `write_output` and `submit_output`. A tool outside an agent's allowlist gets the same answer as a tool that does not exist; the previous `tool `x` is not in this invocation's allowlist` and bare ``unknown tool `x` `` errors are gone. A `libs.tool` source answers the same way for its own tools and now also honors the invocation's `allowlist`.
- **Breaking:** an agent is given one `toolset`, a `Toolset {policy, tools}` that carries its tools together with its approval policy, in place of the separate `tools` and `tool_approval_policy` fields of `AgentConfig`; a list of tools can no longer be paired with the wrong policy. `ToolApprovalPolicy` is one ordered type with two values, `ReadOnly < Default`; `ToolApprovalRules`, the `Rules` constructor and the `da-policy` wire key are gone (the wire carries the policy's name as `approval_policy`). The toolsets manifest states each toolset's policy with its tools, `{"read_only": {"policy": "ReadOnly", "tools": [...]}}`, and a toolset without a policy, with an unknown policy or with another key does not load; `nefor.read_only_tools` and `nefor.general_tools` are `Toolset` values and the prompt's Toolsets section shows each toolset's policy. The starter's `read_only` toolset gains `mag-write-file`, `mag-preview`, `mag-apply` and `mag-terminate`, so a read-only agent can search with commands. MAG's `keys` now also reads a JSON object.
- **Breaking:** the approval policy is enforced. The tool gate decides every call from the caller's policy, the tool's own assessment of the call (`safe`, `unsure`, `denied`) and the permission mode, policy first: a `ReadOnly` agent's unsure or denied call is rejected in every mode and never asked about; for a `Default` agent an unsure call asks in `safe` and runs in `auto` and `yolo`, and a denied call is rejected in `safe` and `auto` and runs in `yolo`. This changes `auto` (it used to deny what `safe` would ask about) and `safe` (a command the classifier denies is now rejected instead of shown). Read-only-ness is no longer derived from an agent's tool list.
- **Breaking:** nothing about approval is implied; what is not defined is an error. A tool call must state its `approval_policy` (a workflow node's own request is marked `workflow_node` instead). `libs.tool` tools define `prepare` (a function or `tool.nothing`) and `execute` (formerly `handler`), and an `approval` entry for every policy with `decide` (`"safe"`, `{ "unsure", reason }`, `{ "denied", reason }` or a function of the arguments) and `describe` (text appended to the tool's description for an agent with that policy, or `tool.nothing`); a definition that leaves any of it out fails when the tool is defined, and a decision that cannot be made ends the call with an error. The gate asks a tool's source to prepare and assess a call (`<source>.tool.prepare`, answered by `tool.prepared`) before it sends `<source>.tool.invoke`. Its `--auto`, `--prompt` and `--default` flags and the starter's `tool_gate.auto_tools` / `prompt_tools` / `default_action` settings are removed; `--deny <tool>` stays. A configuration that chooses what a Nefor tool means for a `Default` agent says so where it builds the tool: `decide_write` for `basic_actor_spec`, `decide_create` and `decide_open` for `git_worktree_actor_spec`.
- **Breaking:** a workflow is approved as a unit. `mag-apply` writes the file, compiles it, and is decided from every node of the compiled workflow under the applying agent's policy: commands are classified, an agent with a wider policy is a rejection, and the error lists every offending node. Each MAG kernel factory declares what its nodes mean for approval, and a node the runtime did not describe is an error. The nodes of a running workflow are no longer decided or asked about one by one. `process.exec` arguments are now classified as the equivalent quoted shell command instead of failing closed.
- **Breaking:** `libs.tool-validator` is now the command classification, `classify_command(policy, command)` and `classify_argv(policy, argv)`; `build` takes `classifier` (was `shell_classifier`), `classifier_args` (the classifier's flags for every approval policy, with no built-in defaults) and `command_fastpaths` (was `shell_fastpaths`, now `function(command, policy)`), all required. `read_only_tools`, `auto_approve_tools` and `process_fastpaths` are removed. A classifier that cannot be run or exits with an unknown status is an error for the call, not a denial. The starter pins `da` 0.3.0: the `ReadOnly` policy uses its read-only rules only, the `Default` policy the wide rules plus the `--ml` model, and no mode flag is passed.
- **Breaking:** every calling node now has its own MAG workspace instead of one folder shared by the whole session: `<session>/mag/runs/<run id>/<logical node path>/`, inside the run folder that already holds the runtime's outputs and nested as the sidebar shows the run. The rule does not depend on the kind of node: the workspace belongs to the logical node that directly contains the calling actor, resolved from the kernel-stamped invocation provenance, and each prompt names its own directory. `mag-write-file`, `mag-preview`, and `mag-apply` reach only the caller's workspace, so one caller can no longer overwrite, read, or run another's source, and session-local imports resolve from the caller's own workspace only. The lead keeps `<session>/mag/lead/` for now. Node names are encoded into folder names without collisions (`%XX` for anything but lowercase letters, digits, `_`, `-`, and a non-leading `.`; a trailing `.output` becomes `%2Eoutput` so a folder never takes a runtime output file's name). Workspaces are created on first write. `libs.mag-workspace` replaces `workspace_dir` and `init_workspace` with `session_dir`, `run_dir`, `node_workspace`, `lead_workspace`, and `ensure_project`, and `libs.mag-context`'s `compose` accepts `workspace_parent`. MAG source written to `<session>/mag/` by an earlier build is not moved.
- **Breaking:** replaced MAG's ambiguous Bash helpers with `nefor.process.exec` and `nefor.shell.script`; removed the canonical `/mode` command; moved reusable Lua mechanisms under `lua/libs`, the bundled distribution under `examples/nefor-agent`, the MAG kernel under the MAG plugin, and installed runtime/plugin paths to generation-aware distribution roots. Pre-0.5 configs and sessions are not guaranteed to resume without migration.

## v0.4.0

- Replaced the reasoner-graph execution path with the run-scoped MAG actor kernel and made MAG the only workflow execution path.
- Made MAG a domain-neutral, typed, namespaced language that lowers library-defined programs to generic artifacts.
- Moved reusable Lua mechanisms out of starter composition into `lua/libs`, leaving starter-owned policy, prompts, and wiring behind explicit extension seams.
- Added MAG shell/tool/provider factories, lifecycle and run observability, output persistence, human injection, cancellation, and a standalone `mag` compiler CLI.
- Added mag-first lead tools and migrated the starter CLI/TUI workflows and deterministic mocks to the kernel contract.
- Added pre-public compatibility/versioning policy and automatic stable tagging from the committed workspace version.

## v0.3.0

- Introduced autonomous permission modes and buffered follow-up input while the chat workflow is busy.
- Added workflow cycles and decoupled bash command reasoning from the scheduler.
- Required builder commits before finalization and avoided creating empty session logs.
- Fixed mock-provider spawn graphs and preserved built-in combinators during the transition.

## v0.2.4

- Preserved system prompts when provider chats are created.

## v0.2.3

- Added native chat compaction, including pending-state UI and preservation of summary items and lead chat identity.

## v0.2.2

- Added reasoning-effort selection.

## v0.2.1

- Removed the fixed timeout from live SSE response bodies.

## v0.2.0

- Added image-aware tools and chat input, exact file edits, bounded/compact read-only tool output, and shared SSE parsing.
- Hardened session replay, event boundaries, interrupted workflows, provider tool-result history, and TUI replay/layout behavior.
- Split local test entry points, added pre-commit/pre-push hooks and the pre-public semantic-version policy, and made chat integration tests deterministic under shared environment state.
- Added Nix flake and Home Manager packaging and version-aware package-manager installation.

## v0.1.3

- **`openai-provider --auth-header NAME`**: send the API key under a non-standard header instead of `Authorization: Bearer …`. Useful for gateways that gate on a custom header (proxies, internal LLM routers). Defaults to `Authorization` so existing setups stay byte-identical.
- **CI automation**: release workflow now auto-bumps `amenocturne/homebrew-tap`'s `Formula/nefor.rb` after each tag push, and supports manual triggers via Actions UI (`workflow_dispatch` with a `tag` input).

## v0.1.2

UX fixes for the chat surface, surfaced during v0.1.1 bring-up.

- **No model configured**: openai-provider's hardcoded `qwen2.5-coder:7b` default is gone — `--model` is now optional, and `chat.create` without one fails with `NoModelConfigured` instead of silently dispatching against a model the user may not have. The error propagates through to the transcript as a clear sentence.
- **Provider error formatting**: HTTP errors from the upstream (Ollama, OpenAI) parse the response JSON's `error.message` field if present so the transcript shows e.g. `Error: HTTP 404: model 'X' not found` instead of the raw JSON envelope.
- **chat.error closes the in-flight node**: agentic_workflow now translates `<provider>.chat.error` to `chat.message.append` (system) and sends a node-result-err to reasoner-graph; the `[thinking…]` spinner stops on chat-create failures (previously hung forever). System messages also clear pending state.
- **Mid-conversation model switch retargets the active chat**: `/model` (or the picker) propagates the active orchestrator chat_id; openai-provider's `model.set` retargets that chat alongside the default. The new model sees the prior turns of the same conversation.
- **Tool-interrupt preserves chat history shape**: when the user interrupts during a tool call, the cancelled tool gets `(tool was interrupted by the user)` as its `tool_result`; any unstarted tools after it get `(tool not run; previous tool call in this turn was interrupted)`. The next turn has a valid OpenAI history shape and the model sees the cancellation context.
- **Session-resume suppresses the tool-permission popup**: replayed `chat.tool.permission_request` envelopes used to open a fresh approval popup even though the original session already had a recorded decision. chat.lua now tracks replay-mode (set by the new `from_resume` flag on `sessions.session_start`) and silently drops permission requests during replay.

## v0.1.1

Hotfix for brew-installed plugin discovery.

- Engine resolves `NEFOR_PLUGIN_DIR` via three new fallbacks before XDG: `<exe>/../share/nefor/plugins` (Homebrew layout), `<exe-dir>` if it bundles `nefor-tui` (in-tree dev), and only then `$XDG_DATA_HOME/nefor/plugins`. The resolved value is set as `NEFOR_PLUGIN_DIR` in the env so `init.lua`'s `bin()` helper sees it without configuration.
- `examples/nefor-agent/init.lua` `bin()` no longer falls back to `<config_parent>/target/debug/<name>`; the engine now propagates the resolved plugin dir.
- `examples/nefor-agent/init.lua` omits `--model` from the openai-provider spawn command when `PROVIDER_MODEL = nil` (instead of emitting a dangling `--model` flag with no value).

## v0.1.0 — initial public release

First public release. Everything in this version is plumbing toward a working agent harness.

- Pure string-bus engine with NCP v0.1 implemented in Lua.
- Declarative TUI plugin (`nefor-tui`) with 15 layout primitives, reconciler, line-diff renderer, and embedded Lua VM.
- OpenAI-compatible HTTP provider (`openai-provider`) targeting any OAI-shape endpoint (Ollama by default).
- Typed reasoner-graph scheduler (`reasoner-graph`) with cycles, per-firing lifecycle, fanout combinators.
- Tool-gate plugin with permission gating + `basic-tools` (`read_file` / `write_file` / `bash`).
- Generic provider/tool type-registry hubs for graph composition.
- Combinator algebra crate plus the corresponding NCP plugin.
- Lua starter config: chat surface composition, agentic workflow, session persistence, `agentic-cli` headless mode.
- XDG-style path resolution: `NEFOR_CONFIG_DIR`, `NEFOR_DATA_DIR`, `NEFOR_PLUGIN_DIR` (CLI flags `--config`, `--data-dir`, `--plugin-dir`).
- Homebrew install via `amenocturne/homebrew-tap`.
