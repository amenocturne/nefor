# IR — graph modifications

## Host artifact boundary

MAG programs return a generic artifact rather than a bare modification:

```json
{
  "format": "nefor.graph-modification/v1",
  "data": {
    "actors": [
      { "id": "answer", "foreign": "nefor.factory.llm", "params": {}, "routes": {} }
    ],
    "messages": [], "kills": [], "rules": []
  }
}
```

`foreign` is a qualified identity exported by the runtime registry contract
snapshot. At the host boundary the plugin resolves it into the kernel's
`factory` field; the kernel then revalidates concrete contracts before applying
the normalized modification below.

The registry snapshot is plain immutable data. Each entry carries `identity`,
the parameter schema, and a `type_scheme` containing explicit variables plus
input and output contracts. Runtime constructors never cross this boundary.

`data.result.from` selects the structural result boundary by actor id and
declared wire tag. The kernel validates that the actor exists and its foreign
contract declares that output. When the selected output is emitted, the kernel
persists it through the ordinary per-node writer and completes the run directly;
no sink actor, concrete terminal type, or route is synthesized.

`mag.load` resolves only against `source_dir` unless the request supplies
`module_roots`. That optional array is passed to MAG as the complete ordered
module search path. Absolute roots are accepted as explicit host inputs;
relative roots resolve beneath `source_dir` and may not escape it. The plugin
does not infer library locations from its installation or configuration.

MAG is a scripting language for the runtime hosted by this plugin. A program
is loaded once — parsed, definitions evaluated, initial modification
validated — and its environment stays resident for the session. The shipped
control-plane can explicitly call resident functions with `mag.eval`, and the
host automatically evaluates registered rule bindings. The IR is the data the
evaluator produces and the kernel folds: a **graph modification**. It is
minimal, carries only basic operations, and must never grow domain concepts or
logic primitives — logic lives in MAG, reached through the evaluator.

## The modification

```json
{
  "actors": [{ "id": "...", "factory": "...", "params": {}, "routes": {} }],
  "messages": [{ "to": "...", "content": {} }],
  "kills": ["..."],
  "rules": [{ "id": "expand", "on": { "actor": "planner", "wire": "tasks.Valid" }, "fn": "expand" }]
}
```

- `actors` — instances to spawn: which resolved factory, with which params,
  under which id.
  Ids are human-readable and namespaced per library fragment
  instantiation: an agent named `docs-explorer` prefixes its internal
  actors, so its provider loop is `docs-explorer.llm` — each instance gets
  its own subtree of names.
- `routes` — kernel-owned typed wiring, sibling of `params` by design:
  params belong to the factory, routes belong to the kernel, and an actor
  never reads its own routes. A map from fully-qualified output type to an
  array of destination ids; the authoring graph's edges dissolve here. A
  union exit becomes multiple keys; one type to many targets is fanout with
  no special casing. See lowering.md for the authoring-to-artifact mapping.
- `messages` — sends: initial activation for new actors, inputs for
  existing ones.
- `kills` — ids to remove. Kill removes actors and voids late outputs; it is
  not currently a general routeable failure output.
- `rules` — immutable initial subscriptions. Each names a concrete source
  actor/output port and a unary pure function in the resident MAG snapshot.
  Deltas cannot add subscriptions or replace the result boundary.

## The fold

Runtime state is a graph; the initial state is NullGraph — empty. Loading a
program applies its initial modification. Later modifications come from
explicit control-plane apply/eval paths or automatic resident rule firing:

```
Graph(0)   = NullGraph
Graph(n+1) = apply(Graph(n), validate(modification(n)))
```

Each actor is treated as a function: the kernel fires its input message,
the actor is a black box until it returns its output. The output routes
along the compiled wiring. The runtime operates over nothing but
modifications — running a workflow _is_ this fold.

## Running a program — registration, then lazy firing

Program start is one fold application, no barrier. A rule produces the distinct
`nefor.graph-delta/v1` artifact: it may spawn, send, kill, and route against
actors already live in the run, but has no result boundary. Applying a program's
_initial_ modification:

1. **Register** every actor in the initial constellation — id, factory,
   params, routes. Registration puts every route and input contract in place
   before any message moves, so senders resolve destinations and partial
   inputs buffer (per-slot, in the firing machine). `mag.actor_spawned` fires
   per actor, here.
2. **Deliver** the initial messages. Each delivery feeds the target's firing
   machine; an actor **constructs at its first satisfied input contract**
   (actor-model.md, Lifecycle) — the factory builds the instance, `mag.ready`
   / `mag.actor_ready` confirm ("began work"), and the activation is
   delivered. The cascade runs synchronously through the constellation in
   data-flow order; a fully synchronous program completes inside the apply.

There is no ready barrier and no readiness deadline: nothing waits on
construction, because nothing constructs until it has work. Actors spawned
later by control-plane applies follow the identical convention — register,
buffer, construct on first firing. A factory that rejects at
construct time (invalid params) surfaces at its first firing as a
`mag.run_failed` escalation, and the host fails the run.

## Run contexts — concurrent runs

Runs are concurrent. Each `mag.execute` gets its own **run context**: an
inventory, routing/firing state, capability correlations, and a modification
log, created at run start (`begin_run`) and dropped at run end — complete,
failed, or superseded. A fresh context IS starting from NullGraph, so nothing
resets between runs; ids are freely reusable across runs, and a run starting
mid-another-run touches nothing outside its own context. Cross-run
interaction does not exist: routes and sends resolve within the run's context
only.

- **Every kernel→control-plane event carries `run_id`** — `mag.run_started`,
  `mag.actor_spawned/ready/killed`,
  `mag.modification_applied/rejected/noop`, `mag.run_complete`,
  `mag.run_failed` — so consumers key overlapping runs apart.
- **Wire-id scoping.** Two runs of the same program author identical actor
  ids, so anything the kernel puts on the shared bus that must resolve back
  to one run is prefixed with the run's scope token `r<K>` (kernel-session
  monotone, never reused): capability correlation ids are `r<K>/cap-<n>`,
  provider chat handles `r<K>/<actor>@r<seq>`. The prefixed strings are
  opaque downstream — consumers match exactly, never parse.
- **Kill semantics are per run.** Ending a run (terminal state, or an
  explicit end) reaps that run's live actors through the fold — kill
  handlers run, abort/cancel envelopes reach the bus — and drops the
  context. Other runs are untouched.
- **Session-boundary reaping.** The engine (and the resident kernel) outlive
  TUI sessions. Beginning a run under a new `session_id` reaps every live
  context left by a different session — the scoped analogue of a global
  reset; concurrent runs of the current session are never touched.

## Firing — when an actor activates

An actor activates when its declared input contract is satisfied. Firing is a
type fact, symmetric to routing: output types decide where results go, input
types decide when the actor runs.

| Input contract    | Fires                                                                            |
| ----------------- | -------------------------------------------------------------------------------- |
| single type `A`   | per message — every arriving `A` is one activation                               |
| union `(A \| B)`  | on any — whichever arrives first activates alone                                 |
| product `(A + B)` | on all — the kernel accumulates components and delivers one assembled activation |

Data flow subsumes dependency: if `A -> B` carries data, B structurally cannot
fire before A's output arrives. There is no separate dependency graph in the
IR — the authoring layer may present dataflow and firing constraints as two
views, but both lower to routes plus input contracts. Ordering without data
is a status-typed route (`mag.Unit`, failure variants), consumed like any
other input.

Dependencies use the same language: "A depends on C finishing" is the edge
`C -> A` carrying `mag.Unit` — an informationless payload whose sole purpose
is to encode the ordering. No second vocabulary exists.

The shipped shell library leans on exactly this algebra: its foreign actor's
input contract is the union `(mag.Unit | mag.Text)`. A Unit firing runs the
command with no stdin; a Text firing supplies upstream stdout. The library
constructs the initial Unit message explicitly, so pipe semantics require no
compiler special case.

- **Slot identity is the incoming edge, not the type.** The kernel assembles
  product activations with per-slot FIFO queues, where each slot is bound to
  its sender at lowering time (messages are id-signed, routes are
  directional, so the binding is known statically). This is what makes
  `(Unit + Unit)` from two different upstreams — or `(Findings + Findings)`
  from two explorers — unambiguous: two completions of the same sender fill
  one slot twice, never two slots. One activation per complete set. The slot
  queues are also the only buffering the lifecycle needs: they accept
  components from the moment the spec registers, and the assembled first set
  is what triggers lazy construction (actor-model.md, Lifecycle).
- **Reserved status types are kernel-emitted.** Route keys matching the
  factory's declared output types dispatch from the returned value; reserved
  route keys `mag.Unit` (successful completion) and `mag.Failed` (generic
  kernel-synthesized failure) are emitted by the kernel as part of applying
  the completion — a factory never returns them and never knows a dependency
  edge exists. Ordinary factory-declared failure outputs such as
  `mag.CommandFailed` are separate tags; `mag.CommandFailed` is the bash
  factory's routable non-zero-exit/capability-error failure, not the generic
  kernel failure tag. The deferred-completion emit kind `mag.failed` is the
  actor-to-kernel ack envelope that carries a failure tag; it is not itself a
  route key. A failure the factory computes is returned with its tag; a
  failure the actor suffers (provider error, kill mid-flight, budget
  exceeded) is kernel-synthesized as `mag.Failed`, so failure routes work
  uniformly regardless of how the failure happened.
- There is deliberately no "fire when X did _not_ happen" — absence is
  expressed as a timeout or a failure route, never a negative predicate.

## Application semantics

- **Serialized and atomic.** Modifications apply one at a time within a run;
  the graph never sees half of one. Concurrent runs interleave at whole-
  modification granularity — each run's fold is its own (Run contexts above).
- **Arrival order is a race, by design.** Nodes complete at their own pace;
  which modification applies first is timing. The race is a feature: spawn
  several agents on the same job with different approaches, and whichever
  control-plane modification applies first wins.
- **First-applied wins.** A completion routes only if its source node is alive
  in the inventory at application time. A node killed between completing and
  applying has its output voided — no window for a dead agent's completion to
  sneak in.
- **Monotone lifecycles.** Every id moves never-existed → alive → dead,
  each transition at most once. Alive means _registered_ — construction is
  lazy and invisible to the lifecycle: a registered-but-unconstructed actor
  counts as alive (duplicate spawns still no-op, sends to it still accepted),
  and killing it just drops the spec (`mag.actor_killed` still fires; no
  final kill message, no instance to receive one). Spawn on a live id:
  no-op. Kill on a dead id: no-op. No-ops are logged — an identical-spec
  duplicate is a race artifact (info), same id with a different spec is
  likely an authoring bug (warning) — but semantics stay uniform: ignored.
- **Every modification is validated before applying**. Compiler-side
  modification validation checks references and shape; the Lua-kernel
  apply-time validator performs the full factory contract checks: contract
  compatibility, id
  uniqueness _within_ the modification (the same id spawned twice in one
  `actors` list is a program bug and rejects), and message targets that
  exist or are created within the same modification. Contract compatibility
  covers routes end to end: every route key must be a declared output of the
  sender's factory (or a reserved / registry-accepted status or failure tag
  such as `mag.Unit` or `mag.Failed`, or factory-specific failures like
  `mag.CommandFailed`), and every destination — spawned in the same
  modification or already live in the inventory (the post-apply actor set) —
  must declare an input port accepting the routed tag. A route no port accepts
  REJECTS the modification with the precise wiring error; it can never reach
  delivery and starve an actor. A rejected initial `mag.execute` modification
  fails the run; a rejected mid-run `mag.apply` modification is an error
  routed to the control plane and normally leaves the run live. Race artifacts
  are never rejections: spawning an id that is already alive is the logged no-op
  above, a send to a dead id drops as a logged no-op at apply, and a route
  at a dead id passes validation (the sender computed it while the target
  lived). Only never-existed targets — message or route — reject: that is a
  typo, not a race. A DYNAMIC mismatch validation cannot see (a message
  whose kind no port of a live target accepts) escalates at delivery as
  `mag.run_failed` instead of silently dropping. One injected kind gets a
  stronger message check: a `mag.ApprovalReply` message must target a
  CONSTRUCTED actor — a reply answers an outstanding request, and a request
  implies a constructed gate — so a reply at a registered-but-unconstructed
  target rejects the modification, while a reply at a dead target stays a
  race-artifact drop (actor-model.md, The approval boundary).
- **The modification log is the run.** Graph state at any moment is a
  prefix of the fold; replay is deterministic even though arrival order was
  not. Debugging is diffing prefixes.


## Modification rejection events

Initial execution and mid-run apply use the same validator but have different
control-plane results:

- Invalid `mag.execute`: the kernel creates the run context, flushes queued
  lifecycle events such as `mag.run_started` and `mag.modification_rejected`,
  then replies with a terminal `mag.run_result { status = "failed", error = ...
  }` and tears the context down. The run does not remain available for later
  applies.
- Invalid mid-run `mag.apply`: the kernel emits and flushes
  `mag.modification_rejected`, replies to the caller with
  `mag.applied { ok = false, error = ... }`, and normally leaves the active run
  live unless some independent completion/failure/kill settles it.

Thus observers may see the same rejection event in both cases, but only the
initial-execute rejection is itself terminal.

## Rules are names, not code

A rule's `fn` is a reference to a function defined in the program's source
snapshot, with declared shape `T -> Artifact` where the artifact format is
`nefor.graph-delta/v1`. Load rejects missing, non-unary, or non-Artifact
functions. A source emission is an ordinary output envelope that must contain
the matching non-empty `kind` and a non-null `value`; extra transport fields
are allowed. The host never guesses from `result`, `content`, or
factory-specific fields. The `value` alone becomes the function argument and
is reified against the function's declared MAG input type before evaluation.

Each run owns a FIFO trigger queue. A source emission is consumed once under
the key `(rule id, source emission sequence)`. The host evaluates a trigger
under the normal MAG fuel budget, validates and atomically applies its delta,
then continues with any nested triggers until quiescence. Evaluation errors,
wrong artifact formats, malformed/rejected deltas, and noncanonical source
payloads fail the run explicitly. Terminal settlement waits for this drain.

- Name-plus-snapshot instead of embedded code: a MAG function closes over
  its defining environment, and re-entering the source snapshot provides
  that environment deterministically — no closure serialization, ever.
- The environment is evaluated once at load and cached; purity makes the
  cache exact.
- **Bounded evaluation**: rule evaluation runs under a step budget;
  exceeding it rejects the modification with an error. Purity means a
  killed evaluation leaves nothing to clean up.
- Load-time checks: every rule's `fn` exists, takes one argument, and
  returns `Artifact`. A typo'd name is a load error, not a
  runtime surprise.
- Inline artifacts cannot invent rule bindings: rule-bearing execution must
  use the resident source snapshot that declared their functions.
- A modification is a plain map — MAG builds it with ordinary data
  constructors and the standard validator checks its shape.

## Kernel operations

Three operations, all environment-side. Modifications reach them through
`actors`, `kills`, and `messages`; the control plane reaches them directly.
Actors reach none of them — an actor receives messages and emits messages,
nothing else.

| Op                           | Meaning                                                                                                                                                                    |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `spawn(id, factory, params)` | Register the spec; the named factory constructs the instance at the actor's first satisfied input contract, and the instance signs all output with `id` and confirms ready |
| `kill(id)`                   | Unilateral removal: unroute, drop the buffered slot inputs, hand the instance (when one was constructed) one final kill message. See actor-model.md                        |
| `send(id, message)`          | Deliver one message to one instance                                                                                                                                        |

Signals are not a fourth operation — a signal is a `send` with a reserved
kind.

## Division of responsibility

| Concern                                                                                                                                | Owner                                                       |
| -------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Parsing, load-time evaluation, validation, rule-function evaluation, id namespacing                                                    | MAG evaluator (`crates/nefor-mag`, resident in this plugin) |
| The fold: applying modifications, lazy construction, slot buffering, routing, correlation, no-op/lifecycle logging, output persistence | Kernel (this plugin's Lua)                                  |
| What an actor actually does with a message                                                                                             | The factory, entirely                                       |
| Capability quirks (provider protocols, aborts)                                                                                         | The capability plugin's own API                             |
