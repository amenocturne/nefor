# Architecture

nefor runs as a small engine plus user-owned Lua composition. The shipped starter layers MAG, providers, tools, approvals, sessions, and interfaces on top of that substrate; those choices are replaceable composition, not engine behavior.

| Layer                         | What it owns                                                                                                                                                                                                                                               | What it avoids                                                                                                                                                                                                         |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Engine / bus                  | Spawning the exact plugin commands registered by Lua, bridging stdio, hosting Lua, routing raw lines through the Lua dispatch hook, stamping in-memory log entries with origin and timestamp, and reporting typed process-termination facts.               | Parsing NCP bodies for routing, owning sessions or selecting their root, writing session jsonl, discovering plugin directories/inventory, deciding whether a plugin exit shuts down the runtime, or owning TLS policy. |
| Plugins                       | Self-contained capabilities over stdin/stdout: providers, tools, TUI, MAG runtime, registries, test actors. Provider plugins own HTTPS policy and construct clients through `nefor-provider-http`, which adds native system roots to bundled WebPKI roots. | Cross-plugin policy or hard-coded knowledge of how another plugin is used.                                                                                                                                             |
| Lua composition and libraries | Dispatch, NCP handshake/routing semantics, explicit plugin commands and distribution resolution, actor spawning, sessions and their root, lifecycle/shutdown policy, approvals, UI reducers, CLI/TUI surfaces.                                             | Heavy provider/tool implementation that belongs in a process plugin.                                                                                                                                                   |
| MAG                           | Pure namespaced evaluation; libraries define ordinary typed graph data, validation, and lowering into a generic `Artifact`. Nefor's library/runtime boundary uses immutable inline `nefor.mag` v3 program and delta envelopes.                             | Knowing actors, factories, shell, sinks, or Nefor wire types; those live in libraries and runtime contracts. The runtime retains no artifact cache or callable compiler environment.                                   |

At the MAG protocol boundary, `mag.execute` accepts only a version-3 program
envelope and `mag.apply` accepts only a version-3 delta envelope. Capability
actors and pure structural junctions are distinct definitions; typed ports use
nominal actor/junction endpoints, routes live at the modification top level,
and `result.from` selects either endpoint kind without a synthetic output actor.
Programs may carry the single closed `InstantiateDeltaTemplate` operation with
its five expression forms; raw modifications, a general runtime expression
language, and post-compilation function application are not part of version 3.

## MAG authoring and runtime protocol ownership

The `nefor` facade is an explicit authoring allowlist. Low-level graph editing,
factory parameters, tool wires, template machinery, and normalization records
remain available through their direct modules, not the facade. Application
inputs and review vocabulary are local nominal types; no library `Task` or
`Text` shape controls entry adaptation or display. Source previews preserve
compiler evidence, and generic records render as structured values.

`nefor.contracts.ProviderInput` and `TextAnswer` are deliberately different:
their exact nominal identities select provider-context input and terminal-text
output codecs. Failure and process-result contracts describe runtime facts.
`nefor.human` owns human workflow approval and its nominal decisions, while tool
authorization belongs to tool-gate/tool-validator and prelaunch write-plan
review belongs to lead-workflow. These are three independent boundaries.

An authored `ToolApprovalPolicy` selects `Default` (no per-agent override, not
an approval bypass) or `Rules(ToolApprovalRules)`. Agent lowering normalizes
that selection into the private `ToolApprovalRuntimeRules` transport record.
The current rule map is metadata: tool-gate does not consume or enforce it.
Authorization still follows its gate policy and configured validator; a deny
entry alone does not prevent execution.

Authored `Timeout` units normalize once in `nefor.timeout`, shared by process
and shell constructors. The library owns positivity and unit factors; MAG's
general-purpose integer operations provide checked signed-i64 multiplication.
Node construction rejects invalid durations before runtime. Lua validates the
normalized record at the artifact boundary; external process/shell tools still
accept their optional-millisecond wire format, not MAG constructor envelopes.

## The decoupling rule

A plugin capability and the logic that uses it are separate concerns. Adding a new workflow or policy should normally touch Lua composition, Lua libraries, or MAG programs, not provider/tool plugin internals.

Bash-tool test: a plugin should feel like a self-contained utility you could run from a shell, then compose elsewhere. If code names neighboring plugins, rewrites their event shapes, or decides global policy, it is glue; put it in Lua.

## What belongs in Lua

Lua owns behavior that is composition-specific or bus-aware:

- NCP handshake and default routing (`lua/core/ncp.lua`).
- Actor spawning and dispatch wiring (`examples/nefor-agent/init.lua`, `lua/core/actor.lua`).
- Session root selection, persistence, replacement, resume, and replay (`examples/nefor-agent/init.lua`, `lua/libs/sessions`). Rust remains session-blind; the starter defaults the root to `$NEFOR_DATA_DIR/sessions` and permits `NEFOR_SESSIONS_DIR` to replace it.
- Approval and tool validation policy (`lua/libs/tool-validator`, `lua/libs/lead-workflow`).
- Provider/tool adapters and interface reducers. The ChatGPT adapter advertises its six web reads as ordinary routed tools; the provider process executes them through a standalone `/alpha/search` client sharing its Responses HTTP/auth boundary, while Lua and the tool gate retain routing and permission policy. The adapter accepts private web traffic only from the selected gate, Rust revalidates invocation provenance before HTTP, each run-tool shares its companion LLM's conversation identity, and oversized results use the gate's generic retrievable output dump rather than a web-specific persistence path.
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

The typed provider boundary (`structured-output`) is the current use. An
attempt whose text fails schema validation, and the bounded correction prompt
that answers it, are recorded diagnostic: the next round still sees them, and
the surface never shows a rejected attempt's prose, reasoning, or streamed
deltas as an assistant message, nor its correction as a user message. The
surface retracts what a narrowed message already streamed, so the accepted
answer occupies the position the turn's provider round started in, and a turn
that exhausts its correction budget settles as one failure with no
metadata-only answer beside it. Attempt counts and violations remain visible on
the existing `mag.diagnostic` channel, which carries no candidate output.

The provider boundary treats finalized tool arguments as untrusted model output. A call is executable only when its `function.arguments` decodes to a JSON object; empty, malformed, scalar, null, and array values are quarantined. The malformed assistant call is not recorded. Instead, the canonical conversation records a bounded user correction naming the call and diagnostic, then requests another completion. This keeps every reconstructed OpenAI assistant tool call provider-valid across continuation and session replay.

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

Route enforcement is deliberately out of scope: the composition targets a
selection at a provider actor, and that actor decides what it will serve. Nefor
does not template per-model endpoints or carry routing metadata that would let
one provider claim another's models. There is no generic seam that could verify
a qualified pair before the owning provider answers, so an unserved pair
surfaces as that provider's rejection (rollback) rather than as a pre-flight
error. Adding such a seam would be a separate design with its own authority and
lifecycle questions.

### Provider usage

Provider usage is a public composition contract separate from context-window token occupancy. Provider completion `usage` and `conversation.provider.context_usage` describe tokens for a request/model context and feed the context bar; they are not `p.usage` values and are never inferred into quota or money. Completion usage keeps aggregate input/output totals while `context_input_tokens` names only the final request's context occupancy. Providers may add normalized optional billing evidence such as cache reads and writes, reasoning tokens, cache-inclusion semantics, and a returned service tier; absence is distinct from an explicit numeric zero. When one operation makes multiple provider requests, `billing_components` keeps one entry per request so pricing can apply request-size thresholds and effective tiers without treating final-request metadata as aggregate truth. Missing request usage becomes an unavailable component and makes `billing_components_complete` false, so aggregate totals are omitted rather than presented as exact. Final-request occupancy remains independent: it is emitted when the final request is measured and omitted when that request's usage is unavailable. Aggregate overflow likewise makes `aggregate_totals_exact` false and suppresses aggregate totals while preserving the exact components. The ChatGPT Responses adapter records returned cached-read, cache-write, and reasoning details, marks input as cache-inclusive, and never invents cache-write counts the endpoint did not return. A surface may set `config.active.usage.quota = "none"` to state that its configured providers have no user quota at all; the starter then omits quota commands and status values while leaving completion token parsing and accounting unchanged. Missing usage data, including an upstream `usage: null`, never implies this policy.

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

Config roots supply `mag.toml`. Session workspace initialization creates only a
missing minimal `version = 1` manifest, never replacing authored content (even an
invalid manifest). Files and deltas need no per-entry manifest edits; generated
`eval/eval-N.mag` names remain distinct compile identities. The lead retains its
loaded artifact once per session; subsequent turns execute it without rebuilding.
Across sessions the same config project and store can hit. Pending-request
cancellation and session rollover still discard late replies, regardless of build
status. Neither cancellation nor session end evicts inert cache records.

`mag.loaded.build` diagnostics are outside the immutable artifact. They do not
change preview, run ownership, or execute/apply behavior. Source readiness does
not activate an installed generation: adoption requires a separately authorized
compatible runtime/config installation and a new process.
