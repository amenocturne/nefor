# Lowering — graph syntax → modification

MAG source is written as a typed graph (`node`, edges, `:terminal`) but the
runtime consumes a **graph modification** (`{actors, messages, kills, rules}`,
see ir.md). Lowering is the pass that turns the first into the second: it
dissolves edges into per-actor routing, namespaces template instantiations,
and emits the single initial modification a program applies at load.

This document specifies that mapping as the compiler (`crates/nefor-mag`)
performs it today. `compile` returns a `ModificationIr{actors, messages, kills,
rules, hash}` directly (`ir.rs`); there is no intermediate graph-shaped IR on
the wire — the mag plugin serializes the modification straight to `mag.loaded`
and the kernel folds it (ir.md).

Worked example: `../tests/fixtures/two-agents.mag` → `../tests/fixtures/two-agents.modification.json`.

## Load pipeline

```
source ──parse──▶ evaluate defs ──extract graph──▶ validate graph ──▶ lower ──▶ GraphModification ──validate modification──▶ resident env
```

`evaluate defs` expands stdlib functions like `(agent ...)` into concrete
actor constellations. `lower` is this document. `validate` is the same
validator that checks every runtime modification (ir.md). The environment then
stays resident; named functions are available to explicit `mag.eval` calls.

## The actor spec

Lowering emits one actor spec per node — the `ActorIr` quad (`ir.rs`), the same
shape ir.md documents:

```json
{ "id": "...", "factory": "...", "params": {}, "routes": {} }
```

| Field     | Consumer                   | Holds                                            |
| --------- | -------------------------- | ------------------------------------------------ |
| `id`      | kernel                     | namespaced actor id (`docs-explorer.llm`)        |
| `factory` | kernel → factory           | which Lua factory constructs the instance        |
| `params`  | factory (opaque to kernel) | factory setup: model, system, tools, provider, … |
| `routes`  | kernel (opaque to factory) | typed output → destination ids                   |

`routes` is a **sibling of `params`, not nested inside it**. `params` is handed
opaquely to the factory (ir.md: "What an
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

Type tags are produced exactly as `qualify_type` does (ir.rs:66): dotted
names pass through (`generic-provider.FinalAnswer`), bare names get a `mag.`
prefix (`mag.Task`). The kernel dispatches an actor's returned output
by looking up its runtime type in `routes` — an O(1) type dispatch carried by
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
plane's kill/interrupt, not a compiled bound. Agent config keys are a fixed
allowlist (`AGENT_CONFIG_KEYS`, eval.rs): `id`, `model`, `profile`, `provider`,
`system`, and `tools`. An unknown key like `:max-steps` or provider-specific
reasoning knobs rejects at compile, and there is no loop-budget mechanism to
author. Direct `llm` nodes and host overlays support the shipped
`reasoning_effort` parameter; arbitrary provider-specific reasoning knobs are
not forwarded through this MAG path.

| Template-internal name | `docs-explorer` instance    | `code-writer` instance    |
| ---------------------- | --------------------------- | ------------------------- |
| `entry`                | `docs-explorer.entry`       | `code-writer.entry`       |
| `llm`                  | `docs-explorer.llm`         | `code-writer.llm`         |
| `run-tool`             | `docs-explorer.run-tool`    | `code-writer.run-tool`    |
| `tool-result`          | `docs-explorer.tool-result` | `code-writer.tool-result` |

Namespacing rewrites the actor `id`s, internal edge endpoints, boundary input,
and boundary outputs at instantiation. Current load-time lowering emits no
rules; automatic rule firing is not shipped behavior today.

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
structurally: the sink emits nothing downstream, and the run's result reaches
the control plane inline on `mag.run_result` (architecture.md control plane).
A modification has no `terminal` field at all.

### Implicit terminal

`:terminal` is a default, not an obligation. Terminal resolution, in
precedence order:

1. an explicit `:terminal` node in a `(graph …)` — carried through as authored
   (`graph.rs` `extract_graph`);
2. exactly one `sink`-factory node in the graph — `resolve_terminal`
   auto-detects it (several stay an error);
3. otherwise the program terminates at the **last node of the chain** (last
   fragment in appearance order): `append_implicit_sink` appends the canonical
   `sink` actor after that node's output ports, its input contract derived from
   their types, and wires `last -> sink`. The sink is the run-result
   machinery — it signals `mag.RunComplete`, the run's result reaches the
   control plane inline on `mag.run_result`, and (when persistence is active on
   the host) it also writes the final output to a per-node file — so "the last
   node is the terminal" materializes as "the last node's output feeds the
   appended sink".

A bare expression is the degenerate case: `(bash "ls")` alone is a one-node
program — that node is entry and result-producing terminal, and the appended
sink carries its stdout out as the run result.

## Shell defaults — MAG as shell

`->` is the pipe. Three authoring defaults make capability nodes compose like
a shell pipeline, with zero ceremony:

- **Inline node expressions.** `(bash "cmd")` is usable directly inside graph
  forms without a `let` binding. Unbound capability nodes get deterministic,
  readable ids in appearance order — `bash-1`, `bash-2`, … (per-compilation
  counters in the Env, so two loads of one source mint identical ids and an
  identical modification hash). A `let`/`def` binding still renames the node,
  as for any node value.
- **Infix chains.** `((bash "rg foo") -> (bash "sort"))` outside a
  `(graph …)` form evaluates to a subgraph — input port = the first
  fragment's input, output ports = the last fragment's outputs — so chains
  compose inside larger graphs and a bare top-level chain compiles as a whole
  program. Edges are type-checked exactly as `graph` edges are.
- **Bare expression = program.** A program whose value is a node or a
  subgraph (not just a graph) compiles: the implicit terminal (above)
  completes it.

### The bash capability node

`(bash "command")` lowers to one actor of the `bash` kernel factory
(plugins/mag/lua/mag-kernel/factories/bash.lua):

| Aspect  | Contract                                                                                                                                     |
| ------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| params  | `{ command }`                                                                                                                                |
| input   | `(mag.Unit \| mag.Text)` — union, fires on any: Unit is a dependency firing (run the command, no stdin); Text arrives as the command's stdin |
| output  | `mag.Text` (stdout); `mag.CommandFailed` declared for routable failure edges                                                                 |
| deliver | one `capability.invoke` (name `bash`, args `{ command, stdin }`) through the gate; gate policy applies unchanged                             |
| failure | non-zero exit / capability error → failed completion with the stderr detail; unrouted it escalates `mag.run_failed` (run fails loudly)       |

A **source** bash node (no inbound edge) is seeded with `{ kind: "mag.Unit" }`
as its initial activation — the general rule: a source node whose input
contract carries the `mag.Unit` variant fires dependency-style; every other
source keeps the task seed (ir.rs `initial_activation_content`).

## Messages, kills, rules (initial modification)

| Section    | Initial modification content                                                                                                    | Notes                                                                                                                                                                                                                      |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `messages` | one activation per source actor / entry port; for a single-agent graph this is `{to: "docs-explorer.entry", content: {…task…}}` | Delivered after the whole constellation registers (spawns precede messages in the apply); each message satisfies its target's input contract, which constructs the actor and fires it (lazy construction — actor-model.md) |
| `kills`    | `[]`                                                                                                                            | The initial constellation removes nothing. Kill removes actors and voids late outputs; it is not a general routeable failure output.                                                                                       |
| `rules`    | `[]`                                                                                                                            | Static graphs bind no rules — all shipped composition is routes plus input contracts. The kernel rejects non-empty `rules` with `"rules not implemented"`.                                                                 |

## Rules status

The IR can represent rule bindings (`{on, fn}`) and the compiler/resident
evaluator can validate/apply named unary functions through explicit `mag.eval`,
but the shipped kernel fold does not fire rule bindings automatically. A
non-empty `rules` list is rejected at apply (`"rules not implemented"`). Current
load-time lowering emits `rules: []` for static graphs; rule-bearing
modifications can only arrive through hand-authored/eval-produced modification
data and will be rejected by kernel apply today.

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
| control edge (fire `b` after `a`, no data) | status-typed route entry on `a` (`mag.Unit`, failure variants)                                                     |
| all-of join ("fire when A and B done")     | destination actor declares a product input `(A + B)` — no IR element; firing is the input contract (ir.md, Firing) |
| static graph                               | `kills: []`, `rules: []`                                                                                           |
| data-dependent composition                 | not shipped as automatic rule firing; use explicit control-plane apply/eval paths, while non-empty `rules` are rejected by the kernel today |
