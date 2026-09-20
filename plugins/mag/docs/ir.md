# IR — graph modifications

## Factory specialization as ordinary data

Lowered actors contain `factory` and `type_arguments` fields authored by
ordinary actor-specific MAG functions. `type_arguments` is empty for a
non-generic factory and contains concrete structural type descriptors for a
generic factory. The runtime checks identity, arity, registry scheme
instantiation, semantic endpoints, and fixed wire tags before spawning
anything. Dynamic routes to static actors are validated against the live
inventory atomically with new actors.

The artifact carries version-4 semantic type nodes directly: primitives,
qualified nominal applications with their concrete substituted bodies, lists,
maps, ADTs, and ordered products. A named type body may encode its owned fields
as a `record` object, but no standalone record descriptor is valid.
Registry schemes use the same representation plus explicit variables. The
runtime validates every node recursively, substitutes concrete arguments, and
compares the result structurally; it never reparses a display string. Functions,
type tags, artifacts, empty-list placeholders, and unresolved variables cannot
enter a concrete type descriptor.

Registry semantic endpoints are declared as `{wire, type}` pairs. Validation
checks each instantiated pair, so matching the set of wires and the set of
ADT constructors independently is insufficient: swapping two ADT types between
their wires rejects before any actor is spawned. Dynamic routes additionally
require compatibility according to the same Rust `ConcreteType` relation used
while compiling the graph. Lua does not maintain a second ADT/product
compatibility algorithm.
Registration first requires the semantic input/output wire sets to exactly
match the factory's runtime shapes. Every actor carries `type_arguments`; the
executor registry remains authoritative even when the actor came from compiled
MAG.

## Host artifact boundary

MAG serializes the value passed to `artifact` without imposing an envelope.
Nefor's MAG library owns an explicit versioned application envelope:

```json
{
  "format": "nefor.mag",
  "version": 4,
  "kind": "program",
  "program": {
    "initial": {
      "actors": [
        { "id": "answer", "factory": "nefor.factory.llm", "params": {} }
      ],
      "routes": [],
      "messages": [],
      "kills": [],
      "nodes": [{ "path": ["answer"], "members": ["answer"] }],
      "result": {
        "from": {
          "type": {},
          "type_id": "sha256:...",
          "leaves": [
            {
              "port": {
                "endpoint": {
                  "constructor": "ActorEndpoint",
                  "value": { "id": "answer" }
                },
                "wire": "nefor.agent.Result",
                "type": {},
                "type_id": "sha256:..."
              },
              "steps": []
            }
          ],
          "through": []
        }
      }
    },
    "operations": []
  }
}
```

A delta uses the same format/version with `kind: "delta"` and a `delta`
payload. It has no result boundary or operations. The core compiler remains
schema-opaque.

Fields typed as MAG `PackedValue` cross this immutable boundary as
`{"$mag":"packed-value","value":...}`. The plugin removes exactly that outer
compiler-owned envelope at the declared actor-param, message-content, capture,
and template positions. It never recursively interprets the payload, so a
packed nominal value with fields such as `{"type":"sha256:...","value":...}` remains data.

Template message content has the separate nominal `TemplatePayload` boundary:
`{"constructor":"Static","value":{"$mag":"packed-value","value":...}}`
retains `Static` and unpacks only its payload;
`{"constructor":"Expression","value":"expression-id"}` retains the reference
without unpacking or evaluating it. Unknown constructors, extra wrapper fields,
and malformed payloads are rejected with the indexed operation/message location.
Control-plane decoding, preview and actor-overlay inventory follow the same rule
as execution. They operate on a copy, leaving the immutable program intact.
Ordinary initial/delta messages still decode only their `PackedValue` boundary;
user data resembling a constructor or another packed envelope remains data.

`factory` is the qualified registry identity and `type_arguments` supplies its
concrete generic specialization. The plugin passes both fields through
unchanged; the kernel validates them before applying the modification.

The registry snapshot is plain immutable data. Each entry carries `identity`,
the parameter schema, and a `type_scheme` containing explicit variables plus
input and output contracts. Runtime constructors never cross this boundary.

`result.from` is a typed `StoredBoundary`, not an endpoint. The kernel validates
each actor-output leaf and its ordered steps, plus each through flow. When a
selected output is emitted, the kernel applies those steps, persists the
observed value through the ordinary per-node writer, and completes the run
directly; no sink or output actor is synthesized.

`mag.load` resolves only against `source_dir` unless the request supplies
`module_roots`. That optional array is passed to MAG as the complete ordered
module search path. Absolute roots are accepted as explicit host inputs;
relative roots resolve beneath `source_dir` and may not escape it. The plugin
does not infer library locations from its installation or configuration.

MAG is a compilation language for the runtime hosted by this plugin. Each
`mag.load` request parses and evaluates the supplied source in a fresh compiler
session, validates the resulting application envelope, preflights provider
schemas and declarative operations, and returns the exact immutable envelope
with its content hash. No source environment, cache entry, or callable function
handle survives compilation.

Execution and apply are distinct closed boundaries. `mag.execute` accepts only
a version-4 program envelope; `mag.apply` accepts only a version-4 delta
envelope. Raw unversioned modifications and crossed envelope kinds are rejected
before application. A program can carry only the version-4
`InstantiateDeltaTemplate` operation and its closed expression vocabulary; it
cannot carry source, bytecode, arbitrary MAG functions, or a general runtime
expression language.

The concrete modification remains the data the kernel folds. It is minimal and
contains only kernel operations and topology definitions; the declarative operation schema stays in the
Nefor MAG envelope.

## The modification

```json
{
  "actors": [{ "id": "...", "factory": "...", "params": {} }],
  "routes": [
    {
      "from": { "endpoint": {}, "wire": "..." },
      "to": { "endpoint": {}, "wire": "..." },
      "transforms": [{ "constructor": "Project", "value": { "index": 0 } }]
    }
  ],
  "messages": [
    {
      "to": { "endpoint": {}, "wire": "..." },
      "transforms": [{ "constructor": "Unit", "value": {} }],
      "semantic_type": {},
      "semantic_type_id": "sha256:...",
      "content": {}
    }
  ],
  "kills": ["..."],
  "nodes": [
    { "path": ["stage"], "members": [] },
    { "path": ["stage", "agent"], "members": ["agent.llm"] }
  ]
}
```

- `actors` — capability instances to spawn: resolved factory, params, typed
  input/output ports, and opaque runtime id. Actors are the only runtime rows
  and do not own topology routes.
- `routes` — kernel-owned typed wiring between actor endpoints. Each route
  carries an ordered `Transform` list; `Assemble` steps key destination-owned
  FIFO cohorts by structural path and explicit slot.
- `messages` — typed sends to actor ports with the same ordered transform
  representation. Template messages use this shape as well.
- `kills` — actor ids to remove; fixed transforms have no ids to kill.
- `nodes` — presentation-only logical hierarchy. Parent paths must exist,
  complete paths are unique, and every actor in a declaring modification has
  exactly one logical owner. Dots in actor ids have no hierarchy semantics.

A program's `initial` modification additionally has one `result` containing a
`StoredBoundary` of actor-output leaves and through flows. A delta has neither a
result boundary nor operations. Declarative operations are siblings of
`initial` in the program envelope, not fields in a concrete modification.

The ordered transform constructors are deliberately small: `Unit` turns any
successful input into `Unit`; `Project` selects a product index; `Pack` and
`Unpack` cross one nominal ADT constructor; `Assemble` contributes one explicit
slot to a destination-owned FIFO cohort; and `EmptyList` supplies the empty
fixed-list result. The same representation is used on actor routes, initial
messages, template routes/messages, and result-boundary leaves/flows.

Consumers reconstruct the recursive presentation tree from flat paths.
Deterministic best-effort route order affects display only, never firing,
routing, or scheduling.

## The fold

Runtime state is a graph; the initial state is NullGraph — empty. Executing a
program registers its immutable operations and applies its initial modification.
Later modifications come from explicit control-plane delta application or
`InstantiateDeltaTemplate` operation firing:

```
Graph(0)   = NullGraph
Graph(n+1) = apply(Graph(n), validate(modification(n)))
```

Each capability actor is treated as a function: the kernel fires its input message,
and the actor is a black box until it returns its output. The output follows
actor routes after applying their ordered transforms; fixed transforms reshape
values without constructing capability actors. The runtime operates over
nothing but modifications — running a workflow _is_ this fold.

## Running a program — registration, then lazy firing

Program start is one fold application, no barrier. An operation materializes a
concrete delta: it may spawn, send, kill, and route against actors already live
in the run, but has no result boundary or nested operations. Applying a
program's _initial_ modification:

1. **Install** every transformed route, then register every capability actor —
   id, factory, params, and typed ports. The complete topology and actor input
   contracts are in place before any message moves, so senders resolve
   destinations and partial assembly inputs buffer. `mag.actor_spawned` fires
   per actor.
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
  `mag.nodes_declared`, `mag.actor_spawned/ready/killed`,
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
- **Session-boundary reaping.** The engine and long-lived kernel outlive
  TUI sessions. Beginning a run under a new `session_id` reaps every live
  context left by a different session — the scoped analogue of a global
  reset; concurrent runs of the current session are never touched.

## Firing — when an actor activates

An actor activates when its declared input contract is satisfied. Firing is a
type fact, symmetric to routing: output types decide where results go, input
types decide when the actor runs.

| Input contract       | Fires                                                                            |
| -------------------- | -------------------------------------------------------------------------------- |
| single type `A`      | per message — every arriving `A` is one activation                               |
| nominal ADT `Choice` | per message — each complete owner value is one activation                        |
| product `(A, B)`     | on all — the kernel accumulates components and delivers one assembled activation |

Dataflow subsumes dependency: if `A -> B` carries data, B structurally cannot
fire before A's output arrives. There is no separate dependency graph in the
IR — the authoring layer may present dataflow and firing constraints as two
views, but both lower to routes plus input contracts. Ordering without data
is a status-typed route (`mag.Unit`, failure variants), consumed like any
other input.

Dependencies use the same language: "A depends on C finishing" is the edge
`C -> A` carrying `mag.Unit` — an informationless payload whose sole purpose
is to encode the ordering. No second vocabulary exists.

The shipped process and shell libraries lean on exactly this algebra: each
actor has an exact `Unit` input. An unfed node receives the graph's one automatic root
activation; placing the same node behind an incoming `Unit` route suppresses
that bootstrap and makes it dependency-driven. No second pipe or sequencing
rule is required.

- **Assembly identity is destination plus structural path plus slot.** An
  `Assemble` transform creates a destination-owned cohort keyed by the concrete
  actor destination and `Assembly.path`. The cohort owns one FIFO per explicit
  slot; a complete activation consumes one value from each slot in slot order.
  Thus `(Unit, Unit)` from two upstreams — or `(Findings, Findings)` from two
  explorers — remains unambiguous, and one producer can feed distinct slots.
  Two completions from one sender fill the same slot twice, never another slot.
  Incomplete cohorts do not activate an actor or fabricate completion.
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
  REJECTS the modification with the precise wiring error. Product inputs are
  checked over the complete post-apply incoming route multiset: every
  component needs exactly one compatible sender-bound edge, including repeated
  types, so underfill and overfill reject before registration rather than
  parking an actor forever. A rejected initial `mag.execute` modification
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

## Terminal run results

Every canonical `mag.run_result` carries `duration_ms`, a nonnegative integer
measured by the MAG runtime's per-execution monotonic clock. Measurement starts
immediately before the accepted run enters `begin_run`, spans its initial
program and every later apply or capability wait, and freezes once when the run
settles completed, failed, or killed. Preflight `mag.error` responses do not
carry a duration because run execution never began.

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

## Declarative operations are closed data

Version 4 defines one operation: `InstantiateDeltaTemplate`. It subscribes to a
concrete typed source output, captures immutable values, and materializes an
actor-only delta template from exactly five expression forms: `Trigger`,
`Capture`, `Field`, `IntToDecimalString`, and `ConcatStrings`. Template routes
and messages retain ordered fixed transforms; no transform is a template entity
or reference target.

Each run registers its ordered operation list before initial application. A
matching canonical output contributes one trigger occurrence to a run-local
FIFO. The kernel evaluates that occurrence against the exact emitted value,
clones and relocates the template, regenerates canonical route identities,
validates the resulting delta, and applies it atomically. Synchronous nested
emissions append work to the same FIFO rather than re-entering the fold.
Terminal success waits for the FIFO to become quiescent, and an operation
failure defeats a success that raced ahead of it.

The representation is intentionally not extensible at runtime. There is no
general AST, arbitrary object construction, condition, loop, source/bytecode,
post-compilation function application, or artifact cache. Adding another
operation or expression form is a versioned contract change, not dynamic code
loading.

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

| Concern                                                                                                                                  | Owner                           |
| ---------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------- |
| Parsing, compilation, semantic descriptor/protocol helpers, immutable-envelope validation                                                | Rust host / MAG evaluator       |
| Operation preflight, expression evaluation, template materialization/relocation, the atomic fold, firing, routing, lifecycle, settlement | Kernel (this plugin's Lua)      |
| What an actor actually does with a message                                                                                               | The factory, entirely           |
| Capability quirks (provider protocols, aborts)                                                                                           | The capability plugin's own API |
