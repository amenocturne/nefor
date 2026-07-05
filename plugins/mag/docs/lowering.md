# Lowering — graph syntax → modification

MAG source is written as a typed graph (`node`, edges, `:terminal`) but the
runtime consumes a **graph modification** (`{actors, messages, kills, rules}`,
see ir.md). Lowering is the pass that turns the first into the second: it
dissolves edges into per-actor routing, namespaces template instantiations,
and emits the single initial modification a program applies at load.

This document specifies that mapping. It is a design spec for the loader task —
the current compiler (`crates/nefor-mag`) emits the older graph-shaped IR
(`GraphIr{terminal, nodes, edges, hash}`) and cannot yet produce this shape;
the gap list at the end enumerates exactly where.

Worked example: `tests/fixtures/two-agents.mag` → `tests/fixtures/two-agents.modification.json`.

## Load pipeline (target)

```
source ──parse──▶ evaluate defs ──lower──▶ GraphModification ──validate──▶ resident env
```

`evaluate defs` expands stdlib functions like `(agent ...)` into concrete
actor constellations. `lower` is this document. `validate` is the same
validator that checks every runtime modification (ir.md). The environment then
stays resident; rule functions become entry points.

## The actor spec — proposed shape

ir.md documents an actor as `{id, factory, params}`. Lowering needs one more
field, so the target actor spec is:

```json
{ "id": "...", "factory": "...", "params": {}, "routes": {} }
```

| Field     | Consumer                   | Holds                                         |
| --------- | -------------------------- | --------------------------------------------- |
| `id`      | kernel                     | namespaced actor id (`docs-explorer.llm`)     |
| `factory` | kernel → factory           | which Lua factory constructs the instance     |
| `params`  | factory (opaque to kernel) | factory setup: model, system, tools, `max`, … |
| `routes`  | kernel (opaque to factory) | typed output → destination ids                |

`routes` is a **sibling of `params`, not nested inside it** — see the design
decision below. `params` is handed opaquely to the factory (ir.md: "What an
actor actually does with a message — the factory, entirely"); `routes` is read
only by the kernel, which owns routing (ir.md division-of-responsibility
table). Keeping them siblings keeps the factory contract free of routing data
it never reads.

## Edges dissolve into routes

There is no `edges` array in a modification. Every edge becomes an entry in the
emitting actor's `routes` map, keyed by the **fully-qualified output type tag**
that travels the edge, valued by an **array of destination actor ids**.

```
routes : { "<qualified-output-type>": ["<dest-id>", ...], ... }
```

| Graph construct                    | Route encoding                                                                                                                                                                                        |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `a -> b` (single output type T)    | `a.routes["T"] = ["b"]`                                                                                                                                                                               |
| Typed exit                         | The `llm` exit `ProviderOut -> (ToolCalls \| FinalAnswer)` becomes two `routes` keys. The variant that leaves the cycle is a type fact, never a position — actor-model.md forbids position heuristics |
| Fanout: same type T to `b` and `c` | `a.routes["T"] = ["b", "c"]`                                                                                                                                                                          |
| Back-edge (cycle) `counter -> llm` | `counter.routes["ProviderOut"] = ["llm"]` — no different from a forward edge                                                                                                                          |
| Terminal sink `out`                | `out.routes = {}` (emits nothing downstream)                                                                                                                                                          |

Type tags are produced exactly as `qualify_type` does today (ir.rs:40): dotted
names pass through (`generic-provider.FinalAnswer`), bare names get a `mag.`
prefix (`mag.Task`). The kernel dispatches an actor's returned output
by looking up its runtime type in `routes` — an O(1) type dispatch, the same
type-driven fanout the `reasoner-graph` plugin already performs, now carried by
the actor instead of a separate edge record.

Routes are **always arrays**, even for a single destination, so fanout needs no
special-casing in the kernel.

## Namespacing via template instantiation

`(agent {:id "docs-explorer" ...})` is a stdlib function returning a subgraph.
Every internal actor it composes is named under the `:id` prefix, and every
route destination that points _inside_ the same instance is rewritten with the
same prefix. Two instantiations of one template therefore occupy disjoint id
subtrees.

The agent template has ONE expansion — the bare agentic cycle: `entry`,
`llm`, `run-tool`, `tool-result`, with the back-edge `tool-result → llm` and
a single output port on `llm`. The loop's terminator is structural: the
llm's output is the union `ToolCalls | OUT`, so a final answer exits through
the output port. Loops are unbounded — stopping a runaway run is the control
plane's kill/interrupt, not a compiled bound. (A `:max-steps` loop bound
shipped once, broken and untested — its exhaust wiring routed a tag no input
port accepted and hung the run — and was removed rather than repaired;
`:max-steps` now rejects at compile like any other unknown agent config key.)

| Template-internal name | `docs-explorer` instance    | `code-writer` instance    |
| ---------------------- | --------------------------- | ------------------------- |
| `entry`                | `docs-explorer.entry`       | `code-writer.entry`       |
| `llm`                  | `docs-explorer.llm`         | `code-writer.llm`         |
| `run-tool`             | `docs-explorer.run-tool`    | `code-writer.run-tool`    |
| `tool-result`          | `docs-explorer.tool-result` | `code-writer.tool-result` |

Namespacing rewrites three things at instantiation: (1) each actor `id`, (2)
every internal route destination, (3) any rule `on` id the template binds. The
prefix is one segment; rule-driven instantiation (for-each fanout) extends the
namespace further with item indices (ir.md: `docs-explorer.sub.0`, …), applied
by the rule function, not by load-time lowering.

**Boundary ports.** A subgraph exposes one input port and one output port. The
port is not an actor — it is a handle the caller wires. Composition resolves it
to concrete internal actor ids:

- input port → the internal actor that receives the boundary input (`entry`).
- output port → the set of internal actors that emit the boundary output type
  (just `llm` for an agent; a template with several boundary emitters resolves
  to all of them).

So the graph-level edge `explorer -> writer` dissolves into a route on **every**
actor behind `explorer`'s output port:

```
docs-explorer.llm.routes["generic-provider.FinalAnswer"] = ["code-writer.entry"]
```

## Cycles and typed exits

The agentic loop is a cycle; it needs zero rules. All of its control flow is
static typed wiring:

| Concern                   | Lowering                                                                                                                                                                                              |
| ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cycle                     | Ordinary routes that happen to point back: `tool-result.routes["ProviderOut"] = ["llm"]`. No cycle marker in the IR                                                                                   |
| Typed exit                | The `llm` exit `ProviderOut -> (ToolCalls \| FinalAnswer)` becomes two `routes` keys. The variant that leaves the cycle is a type fact, never a position — actor-model.md forbids position heuristics |
| Per-instance disjointness | Because ids are namespaced, `docs-explorer.tool-result` and `code-writer.tool-result` are independent instances with independent back-routes to their own `llm`                                       |

Cycles are legal as-is — there is no load-time boundedness check. Every cycle
already has a typed exit (the llm's `FinalAnswer` variant); stopping a run
that never reaches it is the control plane's kill/interrupt.

## The program sink

The graph's `:terminal` node lowers to a `sink` actor with `routes: {}`. There
is **no `terminal` field** in a modification — terminality is expressed
structurally: the sink emits nothing downstream, and its per-node output file
is what the control plane reads as the run result (architecture.md control
plane). The old `GraphIr.terminal` string (ir.rs:8) is dropped.

## Messages, kills, rules (initial modification)

| Section    | Initial modification content                                                                          | Notes                                                                                                                                                                                                                         |
| ---------- | ----------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `messages` | one activation to the entry port of the first agent: `{to: "docs-explorer.entry", content: {…task…}}` | Delivered after the whole constellation registers (spawns precede messages in the apply); it satisfies `docs-explorer.entry`'s input contract, which constructs the adapter and fires it (lazy construction — actor-model.md) |
| `kills`    | `[]`                                                                                                  | The initial constellation removes nothing. Kills appear in _rule-produced_ modifications (e.g. the race pattern: first completion's rule kills the losers)                                                                    |
| `rules`    | `[]`                                                                                                  | A fully static graph binds no rules — all routing is wiring. Rules are for data-dependent composition only                                                                                                                    |

### Where rules would attach

Rules are `{on: <actor-id>, fn: <name>}` and fire when `<actor-id>` returns
(ir.md). They enter the picture only when the _shape_ of the next graph depends
on runtime data. Illustrative (not in this fixture):

- **For-each fanout.** If `docs-explorer` returned a list of sub-tasks and the
  writer should spawn one sub-agent per item:
  `{on: "docs-explorer.llm", fn: "fan-out-writers"}`. The rule receives the
  FinalAnswer, and returns a modification whose `actors` are N namespaced
  `code-writer.<i>.*` constellations plus the `messages` to seed them.
- **Race-and-kill.** Spawn three explorers on the same task, bind
  `{on: "explorer-a.llm", fn: "kill-siblings"}` (and symmetric rules) so the
  first to finish returns a modification with `kills: ["explorer-b", …]`.

The rule `fn` is a name into the program's source snapshot with declared
contract `NodeOutput -> GraphModification`; load-time checks that it exists, is
unary, and matches the contract (ir.md). None of that machinery exists yet.

## Master mapping table

| MAG construct                              | Modification element                                                                                               |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `(node "T" {…} : IN -> OUT)`               | one actor `{id, factory: "T", params: {…}, routes}`                                                                |
| `(agent {:id N …} : IN -> FinalAnswer)`    | a namespaced constellation of actors under prefix `N.`                                                             |
| edge `a -> b`                              | entry in `a.routes` keyed by the crossing type                                                                     |
| union output + its outgoing edges          | multiple keys in the source actor's `routes`                                                                       |
| fanout (one type, many targets)            | one `routes` key with a multi-element array                                                                        |
| cycle back-edge                            | an ordinary `routes` entry pointing upstream                                                                       |
| `:terminal out` (sink)                     | `sink` actor with `routes: {}`; no `terminal` field                                                                |
| initial input                              | one entry in `messages` to the entry port                                                                          |
| control edge (fire `b` after `a`, no data) | status-typed route entry on `a` (`mag.Completed`, failure variants)                                                |
| all-of join ("fire when A and B done")     | destination actor declares a product input `(A + B)` — no IR element; firing is the input contract (ir.md, Firing) |
| static graph                               | `kills: []`, `rules: []`                                                                                           |
| data-dependent composition                 | `rules: [{on, fn}]` binding a MAG function                                                                         |

## Compiler gap list — `crates/nefor-mag`

Every point where the compiler today cannot produce this lowering. This scopes
the loader-modifications task.

| #   | Gap                                                                                                                                                                                                                                                                                                                                         | Where                                                   | Needed                                                                                                                                                      |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **No modification IR.** Only `GraphIr{terminal, nodes, edges, hash}` is emitted; `compile` ends at `ir::normalize`.                                                                                                                                                                                                                         | `ir.rs:7-13`, `lib.rs:compile`                          | New `ModificationIr{actors, messages, kills, rules}` type + a lowering pass replacing/after `normalize`                                                     |
| 2   | **Edges are first-class, not dissolved.** `EdgeIr{from,to,type}` and a top-level `edges` vec; `NodeIr` has no `routes`.                                                                                                                                                                                                                     | `ir.rs:31-38`, `ir.rs:15-22`, `ir.rs:140-152`           | Invert the edge list into per-source `routes: {type: [dest]}`; drop `EdgeIr`                                                                                |
| 3   | **No namespacing.** Ids are `next_node_id()` → `node_N`, overwritten verbatim by the `let`/`def` binding name — a single flat name, no prefixing. `docs-explorer.llm` is unreachable.                                                                                                                                                       | `eval.rs:10-13`, `eval.rs:124-126`, `eval.rs:183-185`   | An instantiation namespace that prefixes internal ids + rewrites route destinations and rule `on` ids                                                       |
| 4   | **No `agent` stdlib fn / subgraph-returning fn / splicing.** A bare `(agent {…} : …)` hits function application and fails (undefined). Even a user `fn` returning a `Value::Graph` cannot be embedded: `eval_graph` only consumes `Value::Node`, `->`, `:terminal`; a `Value::Graph` in the edge stream is silently skipped, never spliced. | `eval.rs:96-107`, `eval.rs:761-890` (skip at `857-859`) | An `agent` stdlib function; subgraphs as first-class composable values; `graph` splicing that dissolves boundary edges into internal-actor routes           |
| 5   | **No ports on nodes/graphs.** `NodeValue`/`GraphValue` carry `nodes, edges, terminal` — no input/output port handles, so agent-as-node composition has nothing to resolve boundary edges against.                                                                                                                                           | `ast.rs:110-130`                                        | Port metadata on subgraph values (which internal actors are the in/out boundary)                                                                            |
| 6   | **Single-terminal / single-component assumptions.** Exactly one `:terminal`/`sink` required; weak-connectivity enforced. The modification model has no `terminal` field.                                                                                                                                                                    | `graph.rs:59-76`, `graph.rs:79-124`, `eval.rs:862-883`  | Drop `terminal` from emission; re-express terminality as a sink with empty routes. (Cycle/dead-branch/type checks stay useful as pre-lowering validation)   |
| 7   | **No rules / messages / kills emission.** Nothing produces any of the three; no initial-activation message synthesis.                                                                                                                                                                                                                       | whole of `ir.rs`                                        | Emit `messages` (initial activation), `kills: []`, `rules`                                                                                                  |
| 8   | **No rule machinery.** No `NodeOutput -> GraphModification` contract, no load-time rule checks (fn exists / unary / contract), no resident source-snapshot for fire-time evaluation, no JSON↔MAG value bridge for feeding node outputs to rule fns.                                                                                         | absent                                                  | The rules-as-names layer (ir.md); out of scope for the _static_ fixture but required for the model                                                          |
| 9   | **`reasoner` vs `factory` field name.** `NodeIr.reasoner` sourced from `node.node_type`.                                                                                                                                                                                                                                                    | `ir.rs:17`, `ir.rs:110`                                 | Rename to `factory` in the actor spec                                                                                                                       |
| 10  | **Fanout shape differs.** `FanoutIr{in, out}` records that an output is a union but not _where each variant goes_ (that lives on edges).                                                                                                                                                                                                    | `ir.rs:24-29`, `ir.rs:99-105`                           | Merge "is a union" + "per-variant destination" into the single `routes` map; drop the `in` field (input type is the actor's own contract, not routing data) |
| 11  | **Hashing covers the wrong shape.** `normalize` hashes canonical `{terminal, nodes, edges}`.                                                                                                                                                                                                                                                | `ir.rs:154-164`                                         | Canonicalize + hash the modification (sorted actors, sorted route keys and destination arrays) to preserve deterministic hashing                            |

Partially present (reuse, not gaps): `qualify_type` (ir.rs:40) already produces
the route-key tags; `MagType`/`accepts` (types.rs) already resolves which union
variant an edge carries (ir.rs:115-130), the exact computation that assigns a
destination to a `routes` key.
