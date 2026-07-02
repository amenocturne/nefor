# IR — graph modifications

MAG is a scripting language for the runtime hosted by this plugin. A program
is loaded once — parsed, definitions evaluated, initial modification
validated — and its environment stays resident for the session; rule
functions are entry points invoked as nodes complete. The IR is the data
those entry points produce: a **graph modification**. It is minimal, carries
only basic operations, and must never grow domain concepts or logic
primitives — logic lives in MAG, reached through the evaluator.

## The modification

```json
{
  "actors":   [ { "id": "...", "factory": "...", "params": { }, "routes": { } } ],
  "messages": [ { "to": "...", "content": { } } ],
  "kills":    [ "..." ],
  "rules":    [ { "on": "...", "fn": "..." } ]
}
```

- `actors` — instances to spawn: which factory, with which params, under
  which id. Ids are human-readable and namespaced per template
  instantiation: an agent named `docs-explorer` prefixes its internal
  actors, so its provider loop is `docs-explorer.llm` — each instance gets
  its own subtree of names.
- `routes` — kernel-owned typed wiring, sibling of `params` by design:
  params belong to the factory, routes belong to the kernel, and an actor
  never reads its own routes. A map from fully-qualified output type to an
  array of destination ids; the authoring graph's edges dissolve here. A
  union exit becomes multiple keys; one type to many targets is fanout with
  no special casing. See lowering.md for the full mapping.
- `messages` — sends: initial activation for new actors, inputs for
  existing ones.
- `kills` — ids to remove.
- `rules` — bindings: when the actor named `on` returns its output,
  evaluate the MAG function named `fn` with that output and apply the
  modification it returns. Modifications carry rules too, so dynamically
  spawned actors can bind rules of their own.

## The fold

Runtime state is a graph; the initial state is NullGraph — empty. Loading a
program applies its initial modification; from then on every completed node
may produce the next one:

```
Graph(0)   = NullGraph
Graph(n+1) = apply(Graph(n), validate(modification(n)))
```

Each actor is treated as a function: the kernel fires its input message,
the actor is a black box until it returns its output. The output routes
along the compiled wiring, and if a rule is bound to the node, the rule's
function computes the next modification. The runtime operates over nothing
but modifications — running a workflow *is* this fold.

## Firing — when an actor activates

An actor activates when its declared input contract is satisfied. Firing is a
type fact, symmetric to routing: output types decide where results go, input
types decide when the actor runs.

| Input contract | Fires |
|---|---|
| single type `A` | per message — every arriving `A` is one activation |
| union `(A \| B)` | on any — whichever arrives first activates alone |
| product `(A + B)` | on all — the kernel accumulates components and delivers one assembled activation |

Data flow subsumes dependency: if `A -> B` carries data, B structurally cannot
fire before A's output arrives. There is no separate dependency graph in the
IR — the authoring layer may present dataflow and firing constraints as two
views, but both lower to routes plus input contracts. Ordering without data
is a status-typed route (`mag.Completed`, failure variants), consumed like any
other input.

Dependencies use the same language: "A depends on C finishing" is the edge
`C -> A` carrying `mag.Unit` — an informationless payload whose sole purpose
is to encode the ordering. No second vocabulary exists.

- **Slot identity is the incoming edge, not the type.** The kernel assembles
  product activations with per-slot FIFO queues, where each slot is bound to
  its sender at lowering time (messages are id-signed, routes are
  directional, so the binding is known statically). This is what makes
  `(Unit + Unit)` from two different upstreams — or `(Findings + Findings)`
  from two explorers — unambiguous: two completions of the same sender fill
  one slot twice, never two slots. One activation per complete set; the same
  hold-until machinery as the pending mailbox, with "until ready" extended
  to "until complete".
- **Reserved status types are kernel-emitted.** Route keys matching the
  factory's declared output types dispatch from the returned value;
  `mag.Unit` (successful completion) and the failure types are emitted by
  the kernel as part of applying the completion — a factory never returns
  them and never knows a dependency edge exists. A failure the factory
  computes is returned like any value; a failure the actor suffers (provider
  error, kill mid-flight, budget exceeded) is kernel-synthesized, so failure
  routes work uniformly regardless of how the failure happened.
- There is deliberately no "fire when X did *not* happen" — absence is
  expressed as a timeout or a failure route, never a negative predicate.

## Application semantics

- **Serialized and atomic.** Modifications apply one at a time; the graph
  never sees half of one.
- **Arrival order is a race, by design.** Nodes complete at their own pace;
  which modification applies first is timing. The race is a feature: spawn
  several agents on the same job with different approaches, and the first
  completion's rule kills the rest.
- **First-applied wins.** A rule fires only if its source node is alive in
  the inventory at application time. A node killed between completing and
  applying has its output voided — no window for a dead agent's
  modification to sneak in.
- **Monotone lifecycles.** Every id moves never-existed → alive → dead,
  each transition at most once. Spawn on a live id: no-op. Kill on a dead
  id: no-op. No-ops are logged — an identical-spec duplicate is a race
  artifact (info), same id with a different spec is likely an authoring bug
  (warning) — but semantics stay uniform: ignored.
- **Every modification is validated before applying**, with the same
  validator that checked the program at load: contract compatibility, id
  uniqueness against the live inventory, message targets alive or created
  within the same modification. A rejected modification is an error routed
  to the control plane; the run continues.
- **The modification log is the run.** Graph state at any moment is a
  prefix of the fold; replay is deterministic even though arrival order was
  not. Debugging is diffing prefixes.

## Rules are names, not code

A rule's `fn` is a reference to a function defined in the program's source
snapshot, with declared shape `NodeOutput -> GraphModification`. At fire
time the resident evaluator applies it — pure evaluation, same evaluator as
load, with runtime data as the argument. The kernel sees a name in and
plain data out; it never learns what a function is.

- Name-plus-snapshot instead of embedded code: a MAG function closes over
  its defining environment, and re-entering the source snapshot provides
  that environment deterministically — no closure serialization, ever.
- The environment is evaluated once at load and cached; purity makes the
  cache exact.
- **Bounded evaluation**: rule evaluation runs under a step budget;
  exceeding it rejects the modification with an error. Purity means a
  killed evaluation leaves nothing to clean up.
- Load-time checks: every rule's `fn` exists, takes one argument, and
  matches the declared contract. A typo'd name is a load error, not a
  runtime surprise.
- A modification is a plain map — MAG builds it with ordinary data
  constructors and the standard validator checks its shape. Patterns like
  for-each fanout are stdlib functions that return modifications, not IR
  operations.

## Kernel operations

Three operations, all environment-side. Modifications reach them through
`actors`, `kills`, and `messages`; the control plane reaches them directly.
Actors reach none of them — an actor receives messages and emits messages,
nothing else.

| Op | Meaning |
|---|---|
| `spawn(id, factory, params)` | Instantiate via the named factory; the instance signs all output with `id` and confirms ready |
| `kill(id)` | Unilateral removal: unroute, drop the pending mailbox, hand the instance one final kill message. See actor-model.md |
| `send(id, message)` | Deliver one message to one instance |

Signals are not a fourth operation — a signal is a `send` with a reserved
kind.

## Division of responsibility

| Concern | Owner |
|---|---|
| Parsing, load-time evaluation, validation, rule-function evaluation, id namespacing | MAG evaluator (`crates/nefor-mag`, resident in this plugin) |
| The fold: applying modifications, ready barrier, pending mailboxes, routing, correlation, no-op/lifecycle logging, output persistence | Kernel (this plugin's Lua) |
| What an actor actually does with a message | The factory, entirely |
| Capability quirks (provider protocols, aborts) | The capability plugin's own API |
