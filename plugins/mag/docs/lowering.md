# Lowering — library data to a runtime artifact

Actor-specific MAG libraries construct ordinary values containing a runtime
factory identity, explicit concrete type arguments, parameters, and semantic
ports. Semantic MAG types remain separate from runtime wire tags: libraries
choose the wire protocol and generic functions preserve their typed relations.

MAG has no graph syntax or graph-specific lowering pass. It evaluates pure,
typed library code. The shipped Nefor libraries represent capability actors,
actor-addressed ports, transformed routes and messages, operations, and result
selection as nominal data whose semantic fields contain opaque compiler
descriptors. Fixed combinators are anonymous `Transform` values; they are
never runtime definitions. Graph validation delegates compatibility and product
coverage to the compiler rather than interpreting descriptor maps in MAG, then
marks the lowered value as the compilation result:

```mag
artifact(modification_data)
```

The complete path is:

```text
namespaced modules
  -> ordinary typed node, boundary, and route values
  -> nefor.graph.Graph
  -> nefor.graph.validate
  -> Nefor-owned nefor.mag v4 program or delta envelope
  -> Artifact(opaque application value)
  -> runtime binding and defensive validation
```

The envelope schema lives in `mag/lib/nefor/mag.mag`; core MAG remains
schema-opaque. A program envelope contains an initial concrete modification and
an ordered list of operations. A delta envelope contains one concrete delta.
Version 4 defines exactly one operation, `InstantiateDeltaTemplate`, whose
closed expression vocabulary is Trigger, Capture, Field, IntToDecimalString,
and ConcatStrings. Its structural template names capability-actor slots,
local/existing actor references, typed actor ports, routes with ordered fixed
transforms, transformed messages, logical paths, scalar parameter bindings,
and explicit actor_id relocation metadata. It contains no executable MAG,
generic AST, source, bytecode, condition, nested operation, or generic
object-construction facility.

## Authoring layer

`nefor.graph` defines graph data and composition functions. `nefor.actors` and
`nefor.shell` are ordinary libraries that return `nefor.graph.Node` values;
there are no compiler builtins named `agent`, `bash`, `graph`, `subgraph`, or
`sink`.

A typed port records three identities:

- `endpoint`: a nominal `ActorEndpoint`; fixed transforms are not endpoints or
  runtime identities;
- `type`: a compiler-created `TypeTag<T>` witness, used by generic library
  composition and lowered to a complete canonical structural descriptor;
- `wire`: the runtime tag emitted or accepted by the implementation.

This lets an agent node expose `core.types.Result<AgentError, CodeAudit>` on the stable
`nefor.agent.Result` wire. The success type is declared with
`type_tag<CodeAudit>()`; an undeclared or misspelled semantic type fails
compilation. Compatible edges route each selected constructor directly. Closed declarative
operations may subscribe to any typed actor output port. The
compiler neither knows what an LLM is nor invents a coercion.

Actor-specific constructors are ordinary typed functions. They select a
factory identity, make generic arguments explicit as type descriptors, and
call `nefor.graph.actor` with parameters whose type their own signature owns.
Runtime registry contracts arrive through the domain-neutral typed
`host_input` boundary and are checked again by `nefor.graph.validate` before
lowering.

## Runtime artifact

`nefor.graph.lower` produces the concrete initial modification placed inside
the program envelope: capability actors, top-level typed routes with ordered
transforms, typed initial messages with ordered transforms, kills, and
`StoredBoundary` result metadata. Every port is an actor endpoint. Each
explicit initial message retains its destination descriptor as `semantic_type`
even though actor factories still consume `content.kind`. Lowering also gives
every exposed, unfed exact-`Unit` node input one typed bootstrap message.
Consequently any unfed `Node<Unit, T>` is a source boundary; the same node
behind an incoming edge remains dependency-driven. `result.from` is the
terminal `StoredBoundary`: its leaves and through flows carry transforms that
observe actor output directly. Closing a graph does not synthesize an output
actor or route.

The runtime binds each qualified factory identity to an implementation and
revalidates its concrete input/output contract as exact semantic-type/runtime-
wire pairs. Rust's `ConcreteType` relation is the single owner of semantic
edge compatibility and product coverage; Lua validates protocol wiring and
delegates non-trivial semantic checks to that host relation. The runtime
remains authoritative for
typed firing, sender-bound product slots, routing, lifecycle, failure handling,
and result completion.

Runtime expansion is authored as immutable operation data:

```text
typed trigger/captures -> closed expression references -> structural DeltaTemplate
  -> InstantiateDeltaTemplate -> ordered program operation
```

The Lua kernel preflights the complete operation set before initial apply,
registers it immutably in artifact order, and matches every canonical output
against those subscriptions. A run-local FIFO evaluates the five expression
forms against that exact emission's `value`, clones and relocates the template,
regenerates canonical edge identities, validates the complete delta against the
live-plus-new inventory, and applies it atomically. Synchronous emissions enqueue
nested work rather than re-entering the fold. Success becomes visible only when
the operation queue is quiescent; an operation failure defeats a success that
raced ahead of it.

The operation is fully represented by immutable data. Compilation retains no
environment, and execution performs no later MAG function application.

A concrete delta has no result boundary or nested operations. Its transformed
routes may target actors already live in the run; the runtime registry validates
those references against the combined live-plus-new inventory before applying
anything. Delta lowering applies the same bootstrap rule to newly introduced
actors, so an unfed `Unit` input starts once whether it was introduced in the
initial graph or by expansion.

Multi-node pipelines are full `.mag` programs: compose typed nodes through
`nefor.node`, connect the resulting boundary to an output, then pass a
`Graph -> Graph` function to `nefor.artifact.compile`. Each compilation
applies that function to `empty_graph` and validates the result for a fresh
run; it does not retrieve or mutate a stored graph.

## Module resolution

`core/types.mag` has identity `core.types` and is imported with
`import core.types.{}`. Imports are transitive, definitions remain in their canonical
namespace, and each module evaluates once. `mag.load.module_roots` is the
complete ordered search path; ambiguous identities across roots are errors.
