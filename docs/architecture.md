# Architecture

nefor runs as a small engine plus user-owned Lua composition. The shipped starter layers MAG, providers, tools, approvals, sessions, and interfaces on top of that substrate; those choices are replaceable composition, not engine behavior.

| Layer                         | What it owns                                                                                                                                                                                                                                               | What it avoids                                                                                                                                                                                                         |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Engine / bus                  | Spawning the exact plugin commands registered by Lua, bridging stdio, hosting Lua, routing raw lines through the Lua dispatch hook, stamping in-memory log entries with origin and timestamp, and reporting typed process-termination facts.               | Parsing NCP bodies for routing, owning sessions or selecting their root, writing session jsonl, discovering plugin directories/inventory, deciding whether a plugin exit shuts down the runtime, or owning TLS policy. |
| Plugins                       | Self-contained capabilities over stdin/stdout: providers, tool capabilities, TUI, MAG runtime, registries, test actors. Provider plugins own HTTPS policy and construct clients through `nefor-provider-http`, which adds native system roots to bundled WebPKI roots. | Cross-plugin policy or hard-coded knowledge of how another plugin is used.                                                                                                                                             |
| Lua composition and libraries | Dispatch, NCP handshake/routing semantics, explicit plugin commands and distribution resolution, actor spawning, sessions and their root, lifecycle/shutdown policy, approvals, UI reducers, CLI/TUI surfaces.                                             | Heavy provider/tool implementation that belongs in a process plugin.                                                                                                                                                   |
| MAG                           | Pure namespaced evaluation; libraries define ordinary typed graph data, validation, and lowering into a generic `Artifact`. Nefor's library/runtime boundary uses immutable inline `nefor.mag` v4 program and delta envelopes.                             | Knowing actors, factories, shell, sinks, or Nefor wire types; those live in libraries and runtime contracts. The runtime retains no artifact cache or callable compiler environment.                                   |

At the MAG protocol boundary, `mag.execute` accepts only a version-4 program
envelope and `mag.apply` accepts only a version-4 delta envelope. Only
capability actors are runtime definitions: they own opaque IDs, factories,
messages, construction, firing, and lifecycle. Fixed combinators are anonymous
compile-time boundary and route transforms (`Unit`, `Project`, `Pack`, `Unpack`,
`Assemble`, and `EmptyList`). Routes and initial/template messages carry their
ordered transforms; `result.from` is a `StoredBoundary` and observes the
selected actor outputs directly, without a synthetic output actor. Programs may
carry the single closed `InstantiateDeltaTemplate` operation with its five
expression forms; raw modifications, a general runtime expression language,
and post-compilation function application are not part of version 4.

## MAG authoring and runtime protocol ownership

The `nefor` facade is an explicit authoring allowlist over the public modules.
Low-level graph editing, factory parameters, tool wires, template machinery, and
normalization records remain available through their private modules, not the
facade. Application
inputs and review vocabulary are local nominal types; no library `Task` or
`Text` shape controls entry adaptation or display. Source previews preserve
compiler evidence, and generic records render as structured values.

A model that authors MAG sees only the modules its configuration declares
public. `libs.mag-context` takes `public_modules`, an ordered list of MAG source
paths: a plain path is embedded verbatim in the `# MAG references` part of the
prompt, and `{ path = ..., embed = false }` is listed with its path only, to be
read when a task needs it. Each path must be a `.mag` or `.magl` file inside
exactly one of the configured `module_roots`, which gives it the import name
the compiler resolves; anything else fails at startup. Modules that are not
listed stay importable but are never named in the prompt, so "private" means
"not shown", not a language visibility rule. Put what authors write against,
with a description comment next to each type and function, in a public module,
and keep resolution tables, wiring, and lowering in modules beside it.

Nefor's own library follows the same cut. Its public modules hold the real
declarations authors write against: `nefor.contracts` (the data agents and
commands exchange), `nefor.workflow` (nodes, combinators, the Result channel),
`nefor.shell`, `nefor.process`, `nefor.path`, `nefor.worktree`,
`nefor.collections`, `nefor.retry`, `nefor.human`, and `nefor.actors` (agent
construction for a configuration's agent module). The `nefor` facade only
re-exports their names for `import nefor.{...}`. Graph construction, lowering,
and actor wiring are private: `nefor.graph`, `nefor.node`, `nefor.result`,
`nefor.dynamic`, `nefor.artifact`, `nefor.mag`, `nefor.llm` (the actors behind
an agent), `nefor.capability` (single-actor nodes, factory identities and
parameter records), `nefor.wire` (tool-call values and runtime records),
`nefor.timeout`, and `nefor.toolsets`. Imports only point downwards: a private
module may import a public module that declares types only
(`nefor.contracts`), and never one that declares functions, so a private
builder that carries an authored type takes it as a type argument. A type's
nominal identity includes its defining module, so moving a declaration renames
the type for the kernel as well; `Node` and `DynamicList` stay declared in
their private modules and are opaque aliases in the public ones.

Toolset contents are shown from the configuration's own manifest, the
`nefor/toolsets.json` its MAG bindings and tool policy already read.
`libs.mag-context` takes `toolsets = { manifest = <path>, names = { { key,
name }, ... } }`: the configuration supplies only the MAG expression an author
writes for each manifest key, and the tool names are rendered from the file.

`nefor.contracts.ProviderInput` and `TextAnswer` are deliberately different:
their exact nominal identities select provider-context input and terminal-text
output codecs. Failure and process-result contracts describe runtime facts.
`nefor.human` owns human workflow approval and its nominal decisions, while tool
authorization belongs to tool-gate/tool-validator and prelaunch write-plan
review belongs to lead-workflow. These are three independent boundaries.

## Tool approval

An agent is given one `Toolset {policy, tools}` (`nefor.contracts`): the tools
it may call and its `ToolApprovalPolicy` are one value, so the two cannot be
paired wrongly, and that policy is the only statement of what the agent may
do. The policy is one ordered sum type, `ReadOnly < Default`;
`lua/libs/tool/approval.lua` repeats the order once for Lua (a test keeps the
two in step) and is the only place Lua compares policies. A configuration's
toolsets manifest (`nefor/toolsets.json`) states each toolset's policy with
its tools, `{"read_only": {"policy": "ReadOnly", "tools": [...]}}`; a toolset
without a policy, with an unknown policy, or with another key does not load,
in MAG (`nefor.toolsets`) or in the prompt context (`libs.mag-context`, which
renders each toolset with its policy). Agent lowering puts the toolset's
tools and policy on the agent's model and `run-tool` actors; `run-tool` sends
the policy's name as `approval_policy` with every tool call. The wire names a
policy, never the mechanism that classifies commands.

Nothing here is implied. Every owner below either defines its part or the
missing part is an error that names it: there is no default policy, no
default decision, and no configured setting that stands in for a tool.

The chain for a tool call is always the same: the model calls a tool, the gate
decides, and the tool executes or the model receives an error saying why it did
not. Nothing executes before the decision. Three owners take part:

- **The tool says what a call means.** Every tool defines `prepare` and
  `execute`, and an `approval` entry for every policy: `decide`, which is
  `safe`, `{ "unsure", reason }`, `{ "denied", reason }` or a function of the
  call's arguments returning one of them, and `describe`, text for an agent
  with that policy. A tool with no prepare step or nothing to add writes the
  one explicit value `tool.nothing`. A definition that leaves any of this out
  fails when the tool is defined (`libs.tool`), and a decision function that
  returns no valid decision, or raises, ends the call with an error. Where a
  configuration chooses what one of Nefor's tools means for a `Default`
  caller, it passes that choice where it builds the tool: `decide_write` to
  `basic_actor_spec`, `decide_create` and `decide_open` to
  `git_worktree_actor_spec`.
- **The gate decides** (`plugins/tool-gate/src/policy.rs`). A call must state
  its `approval_policy`. The gate asks the tool's source with
  `<source>.tool.prepare`; the source validates the arguments, runs `prepare`,
  then the decision, and answers `tool.prepared`. The gate applies one table
  to the caller's policy, the permission mode and that assessment, policy
  first:

  | assessment | ReadOnly (every mode) | Default, safe | Default, auto | Default, yolo |
  | ---------- | --------------------- | ------------- | ------------- | ------------- |
  | safe       | run                   | run           | run           | run           |
  | unsure     | deny                  | ask the user  | run           | run           |
  | denied     | deny                  | deny          | deny          | run           |

  The table is in the Rust gate because deciding whether a call may execute is
  what the gate is for, and it alone holds the mode, the registry and every
  call in flight; it stays ignorant of what any tool does. A call without a
  policy, an answer without a decision, an unknown decision, or `unsure` /
  `denied` without a reason is an error for the call. The gate's only
  configured setting is `--deny <tool>`, an explicit restriction. After the
  decision it sends `<source>.tool.invoke` under the same id, or asks with
  `chat.tool.permission_request`, or answers the caller with an error and
  tells the source to drop the prepared call. It forwards the caller's policy
  on both requests.
- **Command classification** is `libs.tool-validator`'s
  `classify_command(policy, command)`: one classifier executable, exit status
  0 approve, 1 defer, 2 deny, with the flags for every policy given
  explicitly by the configuration (the starter's are in
  `examples/nefor-agent/tool-validator/init.lua`). `shell.script` decides
  from it; `process.exec` writes its argument vector as the equivalent POSIX
  command with every element single-quoted (`core.shell`) and decides from
  the same function. A classifier that cannot be run, or that exits with any
  other status, raises: the call ends with an error saying classification
  failed, never with a policy answer. The validator's actor only forwards the
  gate's question to the chat surface.

A workflow is the unit of approval. `mag-apply`'s `prepare` writes the file,
its decision function compiles it and folds over every node the kernel would
spawn (`libs.mag-workspace`'s `fold_nodes(decoded, initial, f)`, which also
visits the templates of dynamic workflows), and `execute` runs exactly the
artifact that was assessed. What a kind of node means is declared by the
kernel factory that defines it (`declaration.approval`, checked when the
factory is registered and published as `factory_approvals` on `mag.hello` and
`mag.loaded`; see `plugins/mag/lua/mag-kernel/node-approval.lua`): a fixed
decision per policy for routing, model, waiting and human-approval nodes; for
a command or worktree node, the tool that owns its capability, the params
that are its arguments, and what each param means when a dynamic workflow
binds it at run time; for an agent's `run-tool` node, that a policy wider
than the applying agent's is a denial. A capability node therefore means what
the tool with the same capability says. Denied dominates unsure, which
dominates safe, and every node that is not safe is listed in the error. A
kind of node the runtime did not describe, a capability no tool defines, or a
node that cannot be assessed is an error naming the node, not a result. The
result goes through the table like any other tool's. This holds for every
`mag-apply`, including one made by an agent inside another workflow.

Once a workflow runs, the requests its own nodes make (scripts, processes,
worktrees) are marked `workflow_node` by the kernel and the gate only
transports them: such a call has no agent and no policy, and is not prepared,
decided or asked about again. A model cannot produce that marker; it is set
by the kernel's node factories, and an agent's tool calls come from
`run-tool`, which always states the agent's policy. A call that carries both,
or neither, is an error. Runs start in two places only: `mag-apply`'s
`execute`, after the gate's decision, and the agentic loop, which starts the
lead's turn program. That program is authored by the configuration, not
applied by a model, so it does not go through the gate and its nodes are
treated like those of an approved run; the lead's own tool calls carry the
lead's policy like any agent's.

Per-policy description text is appended in one place: the provider compositor
(`lua/libs/compositors/provider.lua`), where one request's tools meet the
agent's policy (`libs.tool`'s `request_tool_specs`). Every provider request
names its agent's policy; the text travels in the gate's registry beside the
description, and the compositor hands the provider a request definition with
it appended.

Authored `Timeout` units normalize once in `nefor.timeout`, shared by process
and shell constructors. The library owns positivity and unit factors; MAG's
general-purpose integer operations provide checked signed-i64 multiplication.
Node construction rejects invalid durations before runtime. Lua validates the
normalized record at the artifact boundary; external process/shell tools still
accept their optional-millisecond wire format, not MAG constructor envelopes.

## The decoupling rule

A plugin capability and the logic that uses it are separate concerns. Adding a new workflow or policy should normally touch Lua composition, Lua libraries, or MAG programs, not provider/tool plugin internals.

Bash-tool test: a plugin should feel like a self-contained utility you could run from a shell, then compose elsewhere. If code names neighboring plugins, rewrites their event shapes, or decides global policy, it is glue; put it in Lua.

## Lua module hosting

The engine permits ordinary Lua 5.4 native module loading through `require`
and `package.loadlib`, with the ordinary Lua standard libraries available.
Configuration and loaded native modules belong to the same trusted local system.
The composition owns `package.path`, `package.cpath`, dependency declarations,
and installation. The engine has no package-manager interface or knowledge of
individual Lua libraries. Native modules must target the host's Lua ABI and
platform; their build and distribution remain outside the engine. On the supported
macOS and Linux targets, the executable exports its embedded Lua symbols so
native modules resolve the host runtime rather than bringing a second Lua VM.
Pure Lua modules need only a composition-selected `package.path`; native modules
also need a compatible binary in `package.cpath`. The debug library is available as part of the trusted Lua runtime; external
modules may use its introspection facilities.

## Distribution Lua dependencies

The starter's package manager declares its Lua dependencies in
`lua/nefor-pm/nefor-pm-dev-1.rockspec`. Its resolved artifact closure is pinned by
version and SHA-256 in `lua/nefor-pm/dependencies.lock`. Distribution preparation
uses LuaRocks to build a private platform-specific tree before runtime startup;
the engine does not execute or discover this preparation. The tree contains only
the locked closure. Preparation fails, without resolving new versions, when that
closure does not satisfy the rockspec or one of the locked rocks.

Source installers and release bundles carry that tree as `lua-dependencies`
under the immutable runtime root. The starter adds its ordinary Lua 5.4 search
paths through `config.distribution`; `NEFOR_LUA_DEPENDENCIES_ROOT` explicitly
selects another prepared tree for development or an alternate launcher. A
separate composition may select different dependencies and a different manager.

See [source-install prerequisites](source-install.md) for canonical program/version
requirements, native discovery, overrides and isolated acceptance.
`just prepare-distribution-lua-dependencies [tree]` prepares the private tree.
Linux runtime modules
require the system OpenSSL 3 libraries and CA certificates. On macOS preparation
copies non-system dylib dependencies into the tree, rewrites their loader paths,
and signs the modified files; release CI prepares each target separately.
The copied OpenSSL keeps the certificate store compiled into its build; for
Homebrew this is `/opt/homebrew/etc/openssl@3/cert.pem`. On a Mac without that
file, HTTPS downloads fail certificate verification unless `SSL_CERT_FILE`
names a store such as the system `/etc/ssl/cert.pem`.
A generated native tree must not be reused across operating systems or CPU
architectures. Preparation downloads explicit hash-locked HTTPS archives with curl;
the installed package manager's HTTP path does not depend on those tools.

## What belongs in Lua

Lua owns behavior that is composition-specific or bus-aware:

- NCP handshake and default routing (`lua/core/ncp.lua`).
- Actor spawning and dispatch wiring (`examples/nefor-agent/init.lua`, `lua/core/actor.lua`).
- Session root selection, persistence, replacement, resume, and replay (`examples/nefor-agent/init.lua`, `lua/libs/sessions`). Rust remains session-blind; the starter defaults the root to `$NEFOR_DATA_DIR/sessions` and permits `NEFOR_SESSIONS_DIR` to replace it.
- Command classification and plan review (`lua/libs/tool-validator`, `lua/libs/lead-workflow`); see [Tool approval](#tool-approval) for who decides what.
- Tool construction (`lua/libs/tool`). Plugins provide capabilities; tools — name, description, closed parameter schema, display contract, argument validation, `prepare` and `execute`, and what a call means under every approval policy — are built only in Lua with `libs.tool`. A Rust-backed tool is a Lua tool whose `execute` sends a capability request (`libs.compositors.tools.capability_tools_spec` wraps `basic-tools` and `git-worktree`); a pure-Lua tool's `execute` does the work. Every schema is closed and validated by the tool's source before either function runs, so arguments a gate approval replaces are validated too. A tool with an argument rule that deserves plain words adds `validate(args)`, checked with the schema and worded in the same style (the MAG source tools use it for their `file` name). Validation is not tool-gate's job: the gate only decides whether an invocation may execute. A call naming a tool its caller cannot use — one that does not exist, or one outside the invocation's `allowlist` — gets one answer in the same style, ``unknown tool `x`; available tools: a, b``, listing what that caller's model was offered in the order it was offered. The gate words it, because it alone holds both the registry and the invocation's allowlist; a MAG agent's run-tool actor forwards the call with its allowlist and the request-local tool names its provider actor added, and never judges the name itself.
- Provider/tool adapters and interface reducers. The ChatGPT adapter defines its six web reads with `libs.tool` as ordinary routed tools; the provider process executes them through a standalone `/alpha/search` client sharing its Responses HTTP/auth boundary, while Lua and the tool gate retain routing and permission policy. The adapter accepts private web traffic only from the selected gate, Rust revalidates invocation provenance before HTTP, each run-tool shares its companion LLM's conversation identity, and oversized results use the gate's generic retrievable output dump rather than a web-specific persistence path.
- Markdown destination metadata and pointer gesture recognition belong to the TUI plugin; activation policy belongs to optional Lua `on_link`, never to a built-in opener. Generic `tui.open_external` and clipboard effects report outcomes independently. The canonical chat entry adapts activation into its configured extension event hook; extension popup state is separate from correlated canonical popup state, which always has precedence.
- Chat event/key sequencing through `libs.chat.controller`, assembled from named handler groups with `libs.chat.dispatch`. The starter supplies its command handler as one visible group; consumers can use the defaults, wrap or replace a handler with an explicit duplicate policy, or bypass the controller and use `nefor-tui` primitives directly.
- MAG submission/control and workspace management.
- Repository instruction discovery and ordinary skill loading (`lua/libs/instruction-files`, `lua/libs/read-only-tools`). Discovery reports AGENTS.md/CLAUDE.md paths; `read_file` loads their contents. Configurations select skill loaders and custom read tools explicitly. Directory/search commands and Python analysis use the configured process capabilities; internal filesystem primitives remain independent of the callable tool catalog.
- Plugin process and external-interrupt lifecycle policy. Rust reports typed `engine.plugin_process_terminated` and `engine.interrupt_requested` facts; composition decides whether to reconcile accepted work or request `nefor.engine.shutdown { code, reason, grace_ms }`. Reporting an interrupt does not begin teardown. The first actual shutdown request owns the complete request and its single cooperative grace window. Engine-spawned plugins lead dedicated process groups; composition may explicitly grant one terminal-owning plugin foreground authority without giving up that isolation. After the shutdown window Rust terminates remaining groups, restores prior terminal authority where needed, and consumes their exit accounting before returning.

Pure reusable mechanisms live under `lua/core` or `lua/libs`; example opinions and concrete wiring live under `examples/nefor-agent`.

### Conversation authority and turn orchestration

The example deliberately separates durable meaning from transient execution:

- `lua/libs/conversation-manager` is the canonical authority for recorded conversation facts. It validates and sequences those facts, derives provider-neutral projections for the TUI/CLI, and supplies the conversation context used to create model calls. Session replay feeds recorded facts back through this owner to reconstruct state; consumers do not infer a second transcript from provider or workflow traffic.
- `lua/libs/agentic-loop` orchestrates the current lead turn. It queues input, starts the configured MAG turn program, manages ephemeral provider chats, and coordinates interruption and compaction requests. Its queue and active-turn bookkeeping are process state, not a competing conversation record.

This split keeps replay authoritative without making the conversation manager responsible for live workflow scheduling. Surface reducers render conversation-manager projections and separately observe transient workflow state.

Conversation messages carry manager-assigned materialized history paths made of bounded positive integer components. Dropping the final component yields a parent and prefixes yield complete ancestry. The manager allocates the next child component under the canonical head, so rewinding moves only that head and a later submission creates a collision-free sibling; prior branches remain immutable canonical facts. Public conversation and provider projections contain only messages on the current head's ancestry. A pending rewind edit is itself canonical state until the replacement user message starts, allowing session replay to restore the exact draft. Context compactions retain the history path where they were created and apply only when that path is an ancestor of the current head.

Structured message content remains the canonical stored value. A producer that owns the corresponding human-authored text may record it as validated `authored_prompt` metadata on the message start. Conversation-manager preserves both forms and owns their projection: provider context, transcript display, rewind selection, and restored drafts receive the authored text, while generic structured records retain JSON display fallback.

Prompt-bearing frontends use `libs.startup-readiness` as a live-process barrier. A composition supplies `required_provider` as a function over the agentic loop's acknowledged model snapshot, so resume can change the required provider before activation completes. Only a current, non-replay `<provider>.hello` establishes provider liveness; persisted or reconstruction-authored `chat.model.set_ack` events describe selection and cannot release startup.

Tool exchanges may carry optional `completion_delivery` metadata. The lead workflow assigns `sync` when a delayed MAG invocation settles through its original tool exchange, or `async` when that exchange settles with the grace acknowledgment and completion reaches the owner later. The MAG tool boundary preserves the value into the canonical tool-result fact; conversation-manager validates the closed representation and projects it unchanged. The TUI renders it directly rather than interpreting serialized tool output. Older canonical facts without the field remain valid and render without a delivery label.

#### Transcript disposition

Not every recorded message is conversation. A message fact carries an explicit
`visibility`: `transcript` (the default — ordinary conversation) or
`diagnostic` (model context that no surface renders as conversation). The
conversation manager validates it, keeps both dispositions in the context
projection it hands to providers, and lets a message narrow to `diagnostic`
exactly once at its terminal fact — content that already streamed can be
retracted, but nothing recorded as diagnostic is ever promoted back.

The typed provider boundary (`structured-output`) owns a private draft for each
activation and supplies canonical MAG JSON schema plus request-local
`write_output` and `submit_output` definitions. Editing optionally validates the
whole saved draft; invalid edited contents remain saved. Only standalone
submission revalidates and captures the accepted snapshot. Tool calls and
receipts use canonical conversation facts and the ordinary graph continuation;
accepted completion is recorded there for replay without rereading the draft.
A final assistant response without submission remains in the conversation and
receives one diagnostic system reminder per stop, continuing the same activation.

The provider boundary quarantines malformed native argument batches before any
execution or recording of invalid assistant calls. Runtime feedback is a
system message, never human-authored input. Model corrections have no retry
budget; cancellation, drain and transport recovery retain their own semantics.

Request-local `tool_specs` travel with catalog `tools` names through the MAG
bridge, conversation invocation and compositor into provider requests. Native
providers project the catalog and request definitions together. A direct
nonempty owner conflicting with an advertised owner is a configuration error;
no request-local tool becomes a global registration.

Provider-native continuation state belongs to the assistant response that
created it. A provider may return an opaque, provider/model-scoped artifact on
its terminal completion; MAG attaches that artifact to the same canonical
`message_completed` fact as the assistant text and tool calls. The conversation
manager persists it and exposes it only through the private model-context
projection, never through conversation snapshots, display projections, or
stream deltas. On a compatible later request, the provider replaces that one
neutral assistant message with its original native output items. A provider or
model mismatch falls back to the neutral message instead of interpreting a
foreign artifact.

The ChatGPT provider uses this path for Responses reasoning, message, and
function-call output items. Encrypted reasoning therefore remains adjacent to
the call it informed across tool continuations, later turns, and session
replay, without exposing readable chain of thought. Artifacts are incremental
per response rather than repeated full-history snapshots. Native compaction
still replaces its completed history prefix; response artifacts in the neutral
tail continue from that checkpoint.

### Model selection

Selecting a model is a request to the provider that owns it, not a fact the
surface may assume. `lua/libs/chat/model_selection` is the single primitive
behind every entry point — the `/model` picker, `/model <provider> <model>`, and
`/model <model>`:

- The surface records the requested `(provider, model)` pair plus the state a
  failure restores, then emits `chat.model.set`.
- `chat.model.set_ack` is correlated against that pending pair. The matching ack
  adopts provider, model, that pair's reasoning default and that pair's context
  window in one patch, before any turn runs. An ack for another pair — a
  provider hello, a replayed session, a superseded request — changes nothing.
  With no pending selection, an ack is adopted only for the already-active
  provider.
- `chat.model.set_failed`, or the pending provider leaving the connected state,
  restores the captured pair and clears the request. The provider compositor is
  the one place that knows a selection was in flight, so it correlates the
  provider's untargeted turn error into `chat.model.set_failed` rather than an
  anonymous system message.
- `/model <model>` resolves against the catalogs reported by
  `chat.models.listed`. It selects only when exactly one catalog offers that
  model; two or more, or none, produce runnable qualified commands instead of a
  guess. `/model <provider> <model>` is the user's own authority and does not
  require catalog membership, so a model id containing slashes works.

`lua/libs/agentic-loop` mirrors the same request/acknowledgment distinction for
execution policy. A `chat.model.set` records only a pending pair; the effective
provider/model changes only on the matching `chat.model.set_ack`, and rejection
leaves the prior pair intact. Its `model_snapshot()` accessor returns an owned
copy of that acknowledged state. A composition may provide both
`agentic-loop` and `lead-workflow` with a resolver that expands this current
selection into its complete model/profile policy. Each mechanism samples its
resolver exactly once before a fresh root or delegated `mag.execute` and puts a
validated owned snapshot on that run. `mag.apply` inherits the target run
context instead of sampling again. The MAG kernel applies the immutable
snapshot at lazy LLM construction, so runtime expansion cannot observe a later
`/model` selection.

The snapshot may also contain a configuration-resolved map of named model
profiles. MAG exposes a typed profile selector, but the names and their concrete
provider/model/effort mappings remain composition policy. Profile-authored LLM
actors resolve only against the map owned by their run; missing profiles fail
construction before provider invocation, and actors added later inherit the
same snapshot. Ordinary actors continue to use the snapshot's current model.

Compaction policy is orchestration metadata, not a provider request option.
`nefor.contracts` owns `CompactionPolicy` and `CompactionThresholds`; the public
facade and actor module expose aliases of those same nominal types. Fixed
`ResolvedModel` and `ModelSettings` values require an explicit policy, including
`disabled_compaction`. Profile selectors carry a Disabled placeholder until lazy
construction resolves the run-owned profile. Every Lua and Rust execution
snapshot requires an explicit `compaction_policy`: `{ kind = "Disabled" }`, or a
closed object containing `soft_percent`, `step_percent`, and `hard_percent`.
The parser validates and separately copies root and profile policies; missing
or null policies are errors in both. The starter selects Disabled in
`config.active.compaction_policy` and supplies it through its snapshot resolver
for lead turns and dispatched workflows. MAG-authored Disabled remains
`CompactionPolicy.Disabled`; lazy construction lowers it to the explicit snapshot
object without changing compaction capability or notice eligibility. Tool dispatch
shares model identity but does not consume a compaction policy. A root conversation
uses the snapshot's root policy, not its selected worker profile's policy.

### Canonical compaction and asynchronous provider requests

Every MAG provider request, including Disabled-policy actors, waits for canonical
receipts for its preceding conversation writes. A run-scoped broker tracks each
emitted write and its accepted/rejected receipt, prepares the provider capability,
and reads manager-owned context at the required watermark. It is a transport
ledger, not another transcript. Replies are accepted only from the canonical
manager or the matching provider edge; reads and continuations are deferred.

Enabled policy uses the latest applicable successful completed-response TOTAL
measurement divided by the advertised **regular** window, with exact unrounded
comparisons. Missing capacity fails preparation rather than inventing a limit.
Soft milestones coalesce to the highest crossed milestone. A notice becomes
eligible evidence only after its canonical append, inclusion in the actual
submitted context, and successful native response have been acknowledged in the
same epoch/history/provider/model binding. A successful nonterminal native round
can establish delivery even without usage; a later error does not erase that
evidence. A hard threshold has priority over soft notices.

`self_compact` is always advertised with an empty argument object. Without
accepted delivery evidence it returns a deterministic diagnostic. Multiple
eligible self calls coalesce into one operation; ordinary siblings go through the
normal gate and settle first. A batch containing both self-compaction and
`submit_output` is rejected before any draft edit or ordinary tool effect.
Compaction freezes a safe prefix before the assistant tool batch; matched calls
and results remain together in the tail.

Conversation-manager records admission and terminal facts. Admission is not
completion: a continuation waits for the matching recorded completion/failure
and a fresh canonical read. Only a completed commit replaces the opaque context
checkpoint and advances the epoch; failure preserves measurement and eligibility.
The compositor delegates to a replaceable capability/operation adapter and owns
its temporary resource cleanup, not the artifact's contents. Manual `/compact`
queries fresh canonical context and binds the same epoch, history, watermark and
safe cutoff before admission. Cancellation, actor teardown and session changes
detach pending callbacks, settle/cancel owned operations, and ignore late native
results. Replay restores canonical facts without provider effects; a pending
operation requires explicit recovery rather than an automatic restart.

Route enforcement is deliberately out of scope: the composition targets a
selection at a provider actor, and that actor decides what it will serve. Nefor
does not template per-model endpoints or carry routing metadata that would let
one provider claim another's models. There is no generic seam that could verify
a qualified pair before the owning provider answers, so an unserved pair
surfaces as that provider's rejection (rollback) rather than as a pre-flight
error. Adding such a seam would be a separate design with its own authority and
lifecycle questions.

### Provider usage

Provider token accounting, completed-response occupancy, and account quota are separate composition contracts. Completion `usage` retains operation-wide accounting; it does not supply the footer's occupancy. The optional `latest_response_usage` object is `{ kind = "completed_response", total_tokens, input_tokens?, output_tokens? }`: the latest successfully completed response's TOTAL, including output, not a cumulative turn total or an input estimate. A valid reported total takes precedence; otherwise exact input plus output may supply it. Cache and reasoning details are subsets, never extra additions. Missing or malformed measurements remain unknown, while measured zero is valid.

The ChatGPT adapter emits an existing nonterminal `usage` progress event after each successful native Responses round, with `latest_response_usage` and provider/model/request correlation, and carries the latest measurement on final completion metadata. A later failed, incomplete, or unmeasured round does not erase it. The generic provider compositor stages preliminary per-request usage until successful completion; it never derives occupancy from billing aggregates or estimates. MAG preserves the measurement in per-round assistant completion metadata independently of whole-turn aggregates.

Conversation-manager validates active request/provider/model correlation and records `response_usage_recorded` facts at the active history head. Canonical snapshots and deltas, not raw `conversation.provider.context_usage` observations, drive the chat measurement. Replay and rewind select the latest applicable fact or completed assistant metadata on the current history ancestry. New conversations and incompatible provider/model changes clear the displayed measurement; retries, tools, streaming, failed attempts, and successful responses missing usage retain it. Failed compaction retains it; successful compaction invalidates pre-compaction measurements until a new measured response. This is a measured snapshot, not a prediction of the next request's size.

Model listings expose `context_windows` for default/regular capacities and optional `max_context_windows` for advertised maxima. They remain distinct: ChatGPT's `context_length` supplies the default and `max_context_window` supplies only the maximum. Chat caches capacities by provider AND model, clears missing metadata, and adopts or rolls back both with model selection. The footer boundary is `current_context_tokens` (latest completed TOTAL or unknown), `max_tokens` (default capacity), and `max_context_tokens` (advertised maximum). Missing maximum does not imply an extended window.

The starter's `chat/context_bar.lua` supplies the footer opinion: inline used count, stationary centered labels on background-colored regions, and percentage against advertised maximum (or the default in a single-window bar). Extended regions are blue through the default, yellow through the rounded 85%-of-maximum threshold, and red through maximum. The threshold rounds to the nearest 10,000 tokens; for 272,000 default and 872,000 maximum the labels are `272k`, `740k`, and `872k`, so 108,000 used displays `108k` and `12%`. Fill moves underneath labels; only visual fill caps at capacity, so over-limit counts and percentages remain visible. Absent/equal/inconsistent maxima use one default window; small or overlapping regions collapse rather than inventing thresholds. Unknown usage shows `?`, not zero; absent capacity adds neither bar nor percentage. Extended/single-window bars request 28/12 colored cells plus dividers, growing when needed to fit labels. The footer reserves the measurement and quota spans' natural width before allocating the remaining row to gate, model, reasoning, and extension text. Long identity text clips instead of pushing the bar and percentage offscreen. A narrow sidebar layout can leave little or no visible identity text; unusually long quota strings or rows narrower than the measurement itself can still clip. The bar geometry remains capacity-based, not usage-dependent. The regions are display policy, not automatic compaction or worker context limits.

For accounting, providers may add normalized optional billing evidence such as cache reads and writes, reasoning tokens, cache-inclusion semantics, and a returned service tier; absence is distinct from numeric zero. When an operation makes multiple provider requests, `billing_components` retains one entry per request so pricing can apply request-size thresholds and effective tiers. Missing request usage makes `billing_components_complete` false and suppresses aggregate totals; aggregate overflow makes `aggregate_totals_exact` false while preserving exact components. The ChatGPT adapter marks input as cache-inclusive and never invents cache-write counts. Diagnostic `context_input_tokens` estimates do not drive occupancy. A surface may set `config.active.usage.quota = "none"` to omit starter quota commands and values without changing token accounting. Missing upstream usage never implies this quota policy.

The public `p.usage` compositor contract is:

- `usage.exposures = { { usage_id, initial } }` registers exact IDs. Every ID must be `<actor-name>/<local-name>`; duplicate IDs in one actor are invalid, same-owner re-exposure is idempotent after reconnect, and another actor cannot claim the namespace or collide with an existing exact ID.
- `usage.subscription = { usage_id, request_kind, updated_kind, error_kind, extract }` adapts a provider-native account snapshot. The compositor owns requests and extraction.
- `usage.contributions = { { usage_id, extension, byok_extension, currency, event_kind } }` adapts authoritative completion metadata into a provider-owned session value, including deduplication and replay reconstruction.
- `usage.subscribe = { subscription_id, usage_ids }` asks conversation-manager to forward live updates to the named surface. This is composition wiring, not a stored value.
- Values are `{ kind = "subscription", ... }`, `{ kind = "monetary", amount, currency }`, `{ kind = "free" }`, or `{ kind = "unknown", reason? }`. The manager never authors one of these values.

The public manager events are `conversation.usage.expose`/`exposed`/`exposure.rejected`, `query`/`query.forwarded`/`query.unavailable`/`query.rejected`, `subscribe`/`subscribe.forwarded`/`subscription.rejected`, provider reports `snapshot.reported` and `update.reported`, surface results `snapshot` and `update`, and `publish.rejected`. Queries and reports carry `request_id`; subscriptions and updates carry `subscription_id`; forwarded events also carry `requester`, `owner`, and the exact `usage_ids`. Unexposed query IDs produce `query.unavailable { usage_ids, reason }`, not manager-synthesized values. Reports must contain each requested exact ID once, belong to the reporting owner, and use a valid value variant.

Runtime `conversation.usage.*` traffic is live control-plane state and sessions do not persist it. A provider needing reconstruction records only its provider-namespaced durable contribution event in the normal session JSONL. On replay the owning compositor folds those records without contacting the provider or re-emitting contributions. A new session clears that compositor ledger. Account snapshots such as ChatGPT subscription quota remain live provider state rather than reconstructed history. Public surfaces must state the same policy: the starter keeps configured `account_ids` across an in-process session switch and clears configured `session_ids` until their owner reconstructs or reports the new session.

The generic OpenAI-compatible Rust plugin owns transport recovery as part of its HTTP/SSE boundary. Connection failures, HTTP 429, and every HTTP 5xx response share one bounded retry budget. An SSE transport failure after successful headers can replay within that budget only before text, reasoning, or tool-call state exists; once any such state has escaped, replay is rejected to prevent duplicate output or effects. Retry progress remains observable to the composition. The plugin otherwise only parses transport facts: standard token fields remain optional; unknown upstream usage members are preserved under `usage.extensions`, and the upstream completion ID is carried when present. OpenRouter `reasoning_details` arrays and the plaintext `reasoning` / `reasoning_content` fields are retained as mutually exclusive native continuation artifacts, separate from display reasoning. Structured details take precedence; plaintext continuation comes directly from provider fields, never from the display transcript. A later assistant message restores one only when its private provider context matches the same provider instance, exact base URL, model, and artifact format; incompatible context falls back to the provider-neutral message, while malformed compatible context fails rather than silently losing required reasoning. Lua instance configuration supplies additional request-body members and the semantics that interpret extensions. Missing usage or completion identity is not turned into authoritative zero accounting.

Stateful OpenAI-compatible chats retain the producing model with each history entry. Model changes filter native reasoning only in the outgoing request view: an in-flight turn continues with its captured model, late responses retain their original ownership, and switching back restores compatible artifacts. Restoring a chat resolves its model once against the current default before checking its history. Native reasoning remains provisional until `finish_reason`; a clean stream end before that boundary is an explicit failure, and cancellation never publishes a partial native artifact as a completed continuation.

The ChatGPT Responses provider uses a stronger provider-round boundary. Streamed
deltas and native output items are provisional until a semantic terminal event;
a transient stream failure emits an explicit discarded-attempt observation and
replays the provider round. Conversation-manager retains the interrupted attempt
as audit-only `discarded` history while excluding it from provider context, and
the chat projection retracts it from the surface. Local tool calls are not
delivered until the successful round commits, so provisional function-call state
does not imply an external side effect. Recovery is unbounded until cancellation
by default, with an optional elapsed limit owned by provider configuration and a
shared half-open gate that coordinates concurrent turns after an outage.

## Provider HTTPS trust

Network-owning Rust providers construct HTTPS clients through the
`nefor-provider-http` crate. It preserves reqwest/rustls's bundled WebPKI public
roots and adds certificates loaded from the platform trust store (including
macOS Keychain roots). Individual native entries which the platform loader
rejects are counted and logged without certificate contents; if loading yields
no certificates and reports errors, provider startup fails visibly. Hostname
and certificate-chain validation remain enabled. This is provider mechanism,
not engine or Lua policy, and there is no custom-PEM configuration surface.

### Asynchronous engine callbacks

Detached runtime tasks never invoke Lua directly. Process stdout, stderr, and exit observations are serialized per process onto a broker-owned callback channel. Channel readiness is an explicit broker wake source: one broker turn invokes a bounded callback batch under the same single-task Lua ownership as inbound dispatch, then drains every bus event those callbacks appended before returning to the idle select loop. A queued callback is itself retained readiness, so arrivals immediately before select cannot lose their wake; dropping the broker receiver during shutdown prevents later tasks from entering a torn-down VM.

## Control plane

The lead operates on run statuses and results, not by inspecting every internal message in a graph. MAG run results are delivered inline on bus events, and the lead-workflow tools expose graph status and output lookup as control-plane conveniences.

Persistence is not an engine promise. The engine keeps an in-memory log for dispatch/replay while the process is alive. Long-term session and MAG-output persistence are Lua/plugin mechanisms owned by the starter libraries and MAG kernel integration.

## Session MAG project builds

The starter opts into `mag.build` at the lead, file preview/apply, and generated
expression sites. It selects one persistent writable store at
`nefor.fs.data_root() .. "/mag/cache"`, shared across sessions. Config and package
sources stay in their original (potentially immutable) roots; no lead source is
copied into session storage. Compiler identity and exact project inputs partition
the store. Runtime provider/model snapshots remain execute-only overlays.

Composition passes the same policy table to
`agentic_loop.configure { lead_program = { project_build = policy, ... } }` and
`lead_workflow.configure { project_build = policy, ... }`. The latter forwards it
to every MAG source compilation. Policy is `{ cache_dir = "/absolute/writable/path", no_cache = false }`;
`no_cache` is optional. Omitting `project_build` or supplying `false` selects cold
`mag.load`. Settings are copied at configuration time. The shared
`libs.mag-workspace.compile_request(id, project_root, entry, module_roots, policy)`
constructs either request without selecting global paths or altering correlations.

Config roots supply `mag.toml`. Each calling node authors into its own workspace
inside its run's folder under the session's MAG folder (`runs/<run id>/<logical
node path>/`, beside the runtime's output files; the lead is the one exception and keeps `lead/`; see `lua/libs/lead-workflow/README.md`), and that workspace is the project
root of its compilations. A workspace's first write
creates the workspace and only a missing minimal `version = 1` manifest, never
replacing authored content (even an invalid manifest). Files and deltas need no
per-entry manifest edits; generated
`eval/eval-N.mag` names remain distinct compile identities. The lead retains its
loaded artifact once per session; subsequent turns execute it without rebuilding.
Across sessions the same config project and store can hit. Pending-request
cancellation and session rollover still discard late replies, regardless of build
status. Neither cancellation nor session end evicts inert cache records.

`mag.loaded.build` diagnostics are outside the immutable artifact. They do not
change preview, run ownership, or execute/apply behavior. Source readiness does
not activate an installed generation: adoption requires a separately authorized
compatible runtime/config installation and a new process.

## Owner-local delivery failure settlement

`core.envelope.checked_emit` and `checked_emit_as` return engine admission
rejections before publication to the emitting owner. JSON encoding retains the
composition's explicit fail-fast policy; checked emission does not contain
arbitrary application exceptions. Enqueue is not proof of peer consumption.

NCP publishes bus-local `ncp.delivery_failed` observations with `peer`, `phase`
(`admission`, `enqueue`, or `egress`), `extent` (`none` or `unknown`), a bounded
diagnostic, and a bounded list of correlation fields from envelopes it actually
possesses. These observations are never sent to process peers, preventing
recursive failure diagnostics. Default delivery rejection means no enqueue for
that item. An egress callback exception can follow partial effects; NCP never
replays that batch or claims it was atomic.

The lead loop distinguishes current execute rejection from load failure and
accepted-run terminals. It releases the never-started turn's obligation, emits
one correlated request error, clears transient busy state, and follows the
existing queue policy without waiting for a manager terminal. Started, stale,
and duplicate correlations cannot be reclassified as pre-start rejection. The
detached-run registry retains `rejected` admission outcomes separately from
executor terminals, with no fabricated `mag.run_result` or canonical body.
Known MAG delivery rejection settles only the matching invocation. Unknown
egress extent, MAG transport loss, and unattributable MAG queue loss use the
existing authority-loss reconciliation seam: accepted effects remain unknown.

Lua read-only tools and lead-workflow translate rejected output into one small
correlated tool error. Config-provided read-only tools settle at most once,
even if they raise after returning a result. CLI completes only after its
whole-request terminal and session flush barrier. An unsolicited flush reply is
inert; a persistence fault during flushing or rejection of the flush request
produces one explicit persistence error and nonzero shutdown instead of waiting
for an acknowledgment that cannot arrive. Ordinary unrelated process exits
during flushing do not bypass that barrier.

### Local delivery observations and startup replay closure

NCP failure diagnostics never re-enter physical egress. Provider compositors
observe `ncp.delivery_failed` through their actor `receive_msg` subscription;
transport and queue-loss observations still reach the wrapper. The provider
settles its pending completion, compaction and model-selection obligations before
publishing the existing correlated failures. Unidentified loss is unknown, not
an effects-free rejection; compaction may have delivered earlier control items.
Late completion, model and compaction acknowledgments do not restore settled work.

CLI startup returns its operation ID to composition. Composition passes that ID
into `sessions.init(resume_id, request_id)` so replay failures are correlated even
before prompt submission. Startup errors use the same session-flush barrier as
whole-request terminals. A matching flush acknowledgement (including its explicit
persistence error), or an explicitly correlated persistence failure, closes the
frontend; an unrelated persistence observation cannot bypass an active barrier.
