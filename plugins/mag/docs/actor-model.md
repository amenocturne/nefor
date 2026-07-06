# Actor model

## Interface

An actor is anything that can receive messages. That is the whole interface;
it never grows. Messages include signals — signals are ordinary messages with
reserved kinds, nothing special at the bus level.

Actors are lightweight in-memory constructs with workflow-scoped lifecycles.
They are not engine constructs and not processes: the engine knows plugins and
how to execute Lua, nothing more.

From the runtime's point of view an actor is used as a function: fire its
input message, black box until it returns its output. Completion is
returning — per activation; a node activated twice returns twice. What the
runtime does with the returned output (routing, rules) is described in
ir.md.

The kernel holds one actor inventory **per run**: a single map from actor id
to instance, shared across all factories. Spawn inserts, kill deletes, routing
consults nothing else. Runs are concurrent — each `mag.execute` gets its own
run context (inventory, routing/firing state, correlations, modification log),
created at run start and dropped at run end — so actor ids, routes, and sends
resolve within one run only, and two runs of the same program coexist without
touching each other (see Run contexts in ir.md).

**Actors are unaware of the bus.** The mag plugin, like any plugin, receives
every bus message; what it does with them is its own business. It filters and
delivers to actors through the kernel — so the kernel is an actor's entire
world: every message an actor receives arrives from the kernel, every message
it emits goes through the kernel. No actor subscribes to, or even knows about,
the bus underneath.

**Actors hold no lifecycle authority.** Composing, spawning, and killing are
environment operations — actors never spawn actors; composition that depends
on runtime results is expressed as rules evaluated by the environment (see
ir.md). An actor receives messages and emits messages, nothing else.

## Factories

A factory is a Lua-defined constructor: given an actor id and setup params, it
creates the actor and confirms creation with a ready message for that id.
Construction is lazy — the kernel invokes the factory at the actor's first
satisfied input contract, never at modification apply (see Lifecycle).
Factories are the trait layer — abstract shapes that become concrete actors
when MAG instantiates them.

Two obligations:

- **Accept the id, sign with the id.** Every outbound message of an instance
  carries its actor id. With several actors running, each is individually
  addressable and none is affected by messages meant for another.
- **Declare the contract.** Each factory states the input shapes it accepts,
  the output shapes it produces, and the signals it handles. Composition
  type-checks against declared contracts; nothing selects inputs by sniffing
  their shape. In a cyclic composition (the agentic loop), which output exits
  the cycle is a type fact — a declared algebraic type, built from sums and
  products, e.g. `ProviderOut -> (ToolCalls | FinalAnswer)` — never a
  position heuristic. The input side carries firing semantics the same way:
  single type fires per message, union fires on any, product fires on all
  (see ir.md, Firing).

### Construction and delivery

`construct(id, params, emit, deps) -> instance`.

- `id` — the actor id; sign every outbound message with it.
- `params` — authored plain data from the modification (opaque to the kernel).
- `emit` — the outbound sink; the instance's whole world for sending.
- `deps` — kernel-injected capabilities (runtime closures the MAG program can't
  author, e.g. the sink's output writer). Distinct from `params` by design:
  params are data, deps are the kernel's side-channel.

The instance exposes `deliver(activation) -> completion`.

- **activation** is one of
  - graph — `{ shape = "single"|"union"|"product", messages = { { from, tag, message }, … } }`
  - reply — `{ kind = "reply", ref, result, error }` (a correlated capability response)
- **completion** is the return value the kernel applies:
  - `"ok"` / `{ status = "ok" }` — success; the kernel emits `mag.Unit` along the
    actor's dependency edges.
  - `{ status = "failed", failure = <tag>, value }` — a computed failure; the
    kernel emits `<tag>` where the failing actor routes it (composed failure
    handling). An UNROUTED failure does not vanish: the kernel escalates it to
    the control plane as a `mag.run_failed` lifecycle event carrying the
    failure detail (`value.error` when present), and the host fails the run
    (`mag.run_result status:"failed"`).
  - `nil` / `{ status = "pending" }` — deferred (async); completion arrives later
    as a reserved emit.

Declared outputs flow through `emit` (routed by tag); the return value is only
the completion status. Reserved emit kinds the kernel intercepts: `mag.ready`
(the readiness confirm — emitted inside `construct`, which lazy construction
places at the first activation, so it coincides with beginning work),
`capability.invoke` (a correlated request), `mag.complete` / `mag.failed`
(the async completion of a deferred activation), `mag.RunComplete` (the sink's
terminal completion signal, surfaced to the control plane as the
`mag.run_complete` lifecycle event), and `mag.ApprovalRequest` /
`mag.ApprovalCancel` (the human gate's control-plane-bound request/cancel,
surfaced as the run_id-stamped `mag.approval_request` / `mag.approval_cancel`
events — see The approval boundary). Kernel-synthesized status
tags (`mag.Unit` on success, the failure tag) are emitted by the kernel, never
returned by a factory and never declared as outputs — a factory does not know
a dependency edge exists.

**No construct-time emitters — a rule, not a flag.** The only thing a
constructor emits is its `mag.ready` confirm. Actors are driven: every output
is produced inside `deliver`, in response to an activation. Spontaneity lives
in the modification's initial `messages`, never in a constructor — a factory
that emitted data or started timers at construct time would make construction
timing observable behavior, and lazy construction deliberately keeps it
unobservable (a never-activated actor never constructs). All shipped
factories conform; a new factory must too.

## Lifecycle

Construction is lazy: spawn registers, first firing constructs. The
convention:

1. **On spawn request the kernel registers the spec** — id, factory, params,
   routes — in the inventory. Routes and slot buffering exist from this
   moment: senders resolve destinations, and messages to the id are accepted
   whether or not an instance exists. The `mag.actor_spawned` lifecycle event
   fires here, at registration.
2. **Messages feed the id's firing machine immediately** (ir.md, Firing). A
   single or union input contract is satisfied by the first arriving message;
   a product input buffers components in its sender-bound slots until every
   slot holds one. Partial inputs queue in the machine — discriminated by
   actor id and edge, not replayed from the bus — so no separate pending
   mailbox exists.
3. **The factory constructs at the first satisfied input contract.** The
   instance is built, emits its `mag.ready` confirm (surfaced as
   `mag.actor_ready` — ready MEANS "began work"), and the first activation is
   delivered immediately after. Later activations reuse the instance.

An actor whose input contract is never satisfied never constructs: no side
effects, no timers, no provider handles — a routed-but-never-activated actor
(the tool leg of an agent that never calls a tool) costs nothing and never
readies. Externally the lifecycle is unchanged and monotone (never-existed →
alive → dead): a registered-but-unconstructed actor counts as alive —
duplicate spawns no-op, sends to it are accepted.

The buffering lives in the kernel because it is the flip side of kill —
spawn = register + buffer, kill = unroute + drop — and factory-side buffering
would have every factory reimplement the same machinery.

### Activity (control-plane events)

Construction is signaled once, by `mag.actor_ready`. Everything after is a
cycle of activations, and the kernel announces each one's busy window as a
pair of control-plane events (snake_case, run_id-stamped like the rest):

- `mag.actor_busy { run_id, id }` — an activation was delivered to the
  instance (routing's activate, after construct on the first firing). The
  actor is doing work: an llm actor is busy for exactly its provider round,
  a run-tool actor for exactly its tool call.
- `mag.actor_idle { run_id, id, busy_ms }` — that activation's completion
  settled: a sync return, an async `mag.complete` / `mag.failed` ack, or a
  capability reply resolving a pending completion. Failed settles emit idle
  too (alongside the failure's own routing/escalation). `busy_ms` is the
  window's length.

The pair strictly alternates per actor. An actor that fires again immediately
just opens a new window — busy follows idle, never nests — and overlapping
activations extend the one open window instead of emitting a second busy.
Consumers get activity-honest state for free: busy = working, between
busy/idle = constructed but idle (an agent loop's actors visibly take turns).
The cost is two events per activation — accepted; the session log already
carries per-round provider traffic, which dwarfs this.

## Signals

The conventions are Unix-shaped. You can write any actor you want and ignore
all of it — but the system expects the shape, and non-conforming actors lose
the graceful path, not the system its correctness.

| Signal | Analog  | Semantics                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------ | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| kill   | SIGKILL | The kernel unilaterally deletes the id from the inventory and drops its buffered slot inputs — no further messages route to it, nothing waits for it, no handler can veto or delay the removal. A dying instance is then handed one final kill message: handling is optional, but an actor holding live external work (an open provider request) implements the handler to abort it. A kill before construction just drops the spec — no instance exists, so there is no courtesy delivery; `mag.actor_killed` still fires for observability |
| drain  | SIGTERM | Finish or abort current work, flush outputs, then die. The handleable convention actors are expected to implement                                                                                                                                                                                                                                                                                                                                                                                                                            |

The universal set stays aggressively small: each entry taxes every actor
forever. The bar is "the system cannot work without it", not "handy".

### Kill reasons (control-plane events)

Every `mag.actor_killed` lifecycle event carries a `reason` naming why the
kernel killed the id — display semantics for consumers, not mechanics: kill
handlers run, abort envelopes flush, and ordering is identical for every
reason.

| Reason         | Emitted when                                                                                  |
| -------------- | --------------------------------------------------------------------------------------------- |
| `modification` | a kill entry in an applied modification — the mid-run control-plane / rule kill (the default) |
| `run_complete` | run-context teardown after the sink signalled successful completion                           |
| `run_failed`   | run-context teardown after an unhandled actor failure ended the run                           |
| `killed`       | run-context teardown for an outright kill (`mag.kill_run`)                                    |
| `reaped`       | session-boundary sweep: a new session's `begin_run` reaped a stale context                    |

Consumers render `run_complete` teardown kills as completion — the node stays
done, so a successful run never repaints as terminated — and every other
reason as a real kill.

**No injected behavior.** There is no wrapper that composes standard handlers
around actor logic. An actor's source is the whole truth: reading a factory
definition shows exactly which signals it handles and how. Conformance is
verified by reading, not trusted to machinery.

## Cancellation

There is no generic cancellation contract. Each plugin's API is the statement
of what cancellation means for it:

- Pure plugins (request → response, no state, no expensive resources — e.g.
  `da`): nothing to cancel. Fire-and-forget; an unread response is already
  clean. Zero overhead by construction.
- Stateful plugins (e.g. a provider with an open streaming call): the abort
  primitive is part of that plugin's own API, with its own shapes.

Factories that use a cancellable capability write an explicit signal handler
with the plugin-level message shapes inline — low-level details live where
low-level knowledge already is (the factory necessarily knows the plugin's
request shapes; it knows the abort shape too). When several factories share a
stateful plugin, the plugin's Lua-side adapter module owns the messy envelope
once and factories call it: reuse via library, not via protocol.

## Canonical payloads

The message shapes the shipped factories emit and consume, as they exist in
code today. Every outbound message is id-signed (`from = <actor id>`, omitted
below). These are pinned contracts: a producer emits exactly this, a consumer
reads exactly this — no alias fallbacks, no shape sniffing.

| Kind                           | Emitter → consumer            | Payload (beyond `kind`, `from`)                                                                                                             |
| ------------------------------ | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `generic-tool.ToolCalls`       | llm → run-tool                | `calls = { { id, name, args }, … }`                                                                                                         |
| `generic-tool.ToolHandle`      | run-tool → tool-result        | `results = { { id, name, output, error }, … }` (index-ordered to the calls)                                                                 |
| `generic-provider.FinalAnswer` | llm → sink / human            | `result` (raw provider result); `text?`, `final_answer?` (lifted when result is a table)                                                    |
| `mag.ApprovalRequest`          | human → control plane         | intercepted emit, surfaced as the `mag.approval_request` event: `correlation = <id>`, `prompt?`, `subject` (the input message)              |
| `mag.ApprovalReply`            | control plane → human         | injected via a `mag.apply` message; delivered as a graph activation tagged `mag.ApprovalReply`, `message = { approved, content?, reason? }` |
| `mag.ApprovalCancel`           | human (drain) → control plane | intercepted emit, surfaced as the `mag.approval_cancel` event: `correlation = <id>`                                                         |
| `human.Approved`               | human → downstream            | `subject`, `content`                                                                                                                        |
| `human.Rejected`               | human → downstream            | `subject`, `reason`                                                                                                                         |

The llm factory is the provider boundary: it normalizes the provider's native
tool-call shape (`name`/`arguments`, or a nested `function`) into the pinned
`{ id, name, args }` once, so `run-tool` reads `id`/`name`/`args` directly.

### The approval boundary

The human factory is the approval/input boundary, and its two message
directions travel two different channels — neither is graph routing:

- **Request out.** A subject firing the gate's declared input records it as
  pending and emits `mag.ApprovalRequest`. The kernel intercepts the emit
  (there is no downstream actor — the consumer is the control plane) and
  surfaces it as the `mag.approval_request` control-plane event, run_id-stamped
  like every lifecycle event, carrying `from` (the gate's actor id),
  `correlation`, `prompt?`, and `subject`. The chat surface renders it; the
  gate's activation defers (`pending`).
- **Reply in.** The reply originates at the chat surface, not an upstream
  actor: it has no sender edge for a firing slot to bind to, so no factory
  declares an input port for it. The control plane injects it as a `mag.apply`
  modification message — to the gate's id, content
  `{ kind = "mag.ApprovalReply", approved, content?, reason? }`, addressed by
  the event's `run_id` + `from` — and the kernel delivers it by tag past the
  declared ports, directly to the CONSTRUCTED instance (the port bypass). The
  resolved gate emits its typed exit (`human.Approved` / `human.Rejected`,
  ordinary routed outputs) and acks with `mag.complete`.

Pinned edge semantics:

- **A reply at an unconstructed gate rejects the modification.** A reply can
  only answer an outstanding request, and a request is emitted inside the
  gate's first activation — so an outstanding request implies a constructed
  instance. Apply-time validation rejects the injection (the `mag.applied`
  ack carries the error) rather than parking it: a parked reply could resolve
  a FUTURE request the human never saw. Nothing is lost — the gate's own
  pending subject lives in the constructed instance and buffers indefinitely.
- **A reply at a dead gate is a race artifact**: it passes validation and
  drops at delivery as a logged no-op, like any send to a dead target.
- **A late or duplicate reply at a constructed gate with nothing pending is
  ignored** — it is not an activation.

### llm params

Authored data on the llm actor spec (all optional unless noted):

| Param              | Meaning                                                                                                                                                                                                                                                                               |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `provider`         | provider capability name — **required**; construction fails without it                                                                                                                                                                                                                |
| `model`            | provider model id                                                                                                                                                                                                                                                                     |
| `system`           | system prompt; rides the request's `system` field every round                                                                                                                                                                                                                         |
| `tools`            | advertised tool list for the call                                                                                                                                                                                                                                                     |
| `profile`          | orchestration profile name, resolved by the control plane into provider/model/`reasoning_effort` via the params overlay before spawn                                                                                                                                                  |
| `reasoning_effort` | resolved reasoning effort for the call                                                                                                                                                                                                                                                |
| `history`          | transcript seed: an array of provider-dialect messages (role-tagged turns; assistant tool-call turns in the wire shape the transcript records) that becomes the owned transcript's initial contents at construct — every round replays it ahead of the turns the instance accumulates |

`history` is the turn-as-function seam: the lead's turn is a short-lived
kernel program over a persistent chat, `(history, message) -> response`, and
the spawner passes the conversation so far as content via params (the
per-actor params overlay) — never paths, so construct stays I/O-free. Because
the llm actor already replays its full transcript to a fresh provider chat
each round, a seeded prefix and accumulated turns are indistinguishable at
replay.

System precedence: `system` and `history` are orthogonal channels. `system`
is THE system-prompt channel; a system-role entry inside the seed is neither
lifted into the request's `system` field nor stripped — it replays verbatim
as an ordinary leading transcript message. A spawner forwarding a transcript
that already contains the system turn passes it through exactly one channel.

A malformed seed (non-array, entry without a `role`, non-array `tool_calls`)
fails construction with the offending detail; the instance never binds and
the kernel escalates the construct failure as a run failure.
