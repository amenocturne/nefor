# Lowering — library data to a runtime artifact

`Foreign<P,I,O>` lowers its runtime identity plus opaque compiler evidence for
the concrete specialization and instantiated semantic endpoints. Semantic MAG
types remain separate from runtime wire tags: libraries choose the wire
protocol; the compiler proves which `T` instantiated the foreign contract.

MAG has no graph syntax or graph-specific lowering pass. It evaluates pure,
typed library code. The shipped Nefor libraries represent actors, ports,
routes, messages, rules, and result selection as ordinary nominal data, validate
that data, and return the one host-recognized wrapper:

```lisp
(artifact "nefor.graph-modification/v1" modification-data)
```

The complete path is:

```text
namespaced modules
  -> typed values and Foreign<P, I, O> specializations
  -> nefor.graph.Graph
  -> nefor.graph.validate
  -> nefor.graph.lower
  -> Artifact("nefor.graph-modification/v1", data)
  -> runtime binding and defensive validation
```

## Authoring layer

`nefor.graph` defines graph data and composition functions. `nefor.actors` and
`nefor.shell` are ordinary libraries that return `nefor.graph.Fragment` values;
there are no compiler builtins named `agent`, `bash`, `graph`, `subgraph`, or
`sink`.

A typed port records two identities:

- `type`: a compiler-created `TypeTag<T>` witness, used by generic library
  composition and lowered to a versioned structural descriptor;
- `wire`: the runtime tag emitted or accepted by the foreign implementation.

This lets an agent fragment expose a nominal result such as `CodeAudit` while
its LLM actor still emits `generic-provider.FinalAnswer`. The distinction is
declared by the library boundary with `(type-tag CodeAudit)`; an undeclared or
misspelled semantic type fails compilation. The compiler neither knows what an
LLM is nor invents a coercion.

Foreign capabilities are typed values. `nefor.graph.actor` accepts a
`Foreign<P, I, O>`, validates the parameter type, and stores its qualified
identity in graph data. Runtime registry contracts arrive as immutable MAG
inputs and are checked again by `nefor.graph.validate` before lowering.

## Runtime artifact

`nefor.graph.lower` produces only the graph-modification data consumed by the
kernel: actors, typed routes, initial messages, kills, rules, and structural
result metadata. Initial activation is explicit library data. The result is
selected by `{from: Port}`; no terminal actor or sink route is synthesized.

The runtime binds each qualified foreign identity to an implementation and
revalidates its concrete input/output contract as exact semantic-type/runtime-
wire pairs. It remains authoritative for
typed firing, sender-bound product slots, routing, lifecycle, failure handling,
and result completion.

Rule functions use a parallel, deliberately narrower path:

```text
typed source value -> pure unary MAG function -> nefor.graph.Delta
  -> nefor.graph.lower-delta
  -> Artifact("nefor.graph-delta/v1", data)
  -> atomic apply to the same run
```

A delta has no result boundary. Its routes may target actors already live in
the run; the runtime registry validates those references against the combined
live-plus-new inventory before applying anything.

## One-off command expressions

`mag-eval` wraps a fragment expression with the standard artifact pipeline, so
the expression itself is concise:

```lisp
(nefor.shell.command "search" "rg -n TODO src/")
```

Pipelines use the graph library explicitly:

```lisp
(nefor.graph.connect
  (nefor.shell.command "search" "rg -n TODO src/")
  (nefor.shell.pipe-command "sort" "sort"))
```

The wrapper imports `nefor.artifact`, `nefor.graph`, and `nefor.shell`, creates
the initial `Unit` message with `nefor.shell.start-message`, calls
`nefor.graph.finish`, and then `nefor.artifact.compile`.

## Module resolution

`core/types.mag` has identity `core.types` and is required as
`"core.types"`. Imports are transitive, definitions remain in their canonical
namespace, and each module evaluates once. `mag.load.module_roots` is the
complete ordered search path; ambiguous identities across roots are errors.
