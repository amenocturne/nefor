# MAG Language Refactor

MAG is a pure, namespaced data-construction language. Runtime concepts are
provided by libraries and foreign declarations; the compiler does not know
about graphs, actors, factories, sinks, tasks, providers, tools, or shell.

## Compilation

```text
source module + immutable inputs
  -> resolve namespaced module closure
  -> evaluate pure values
  -> library validation and lowering
  -> Artifact(format, data) | diagnostic
```

The top-level program must return an `Artifact`. `Artifact` is the only
host-recognized output wrapper. Its format is a qualified string identity and
its data is an ordinary MAG value. Validation abstractions such as `Validated`
are library-defined.

Compilation is a snapshot boundary. Source modules, host inputs, and every
file reached through `(read path)` are immutable within one loaded program.
The first read snapshots the file's raw contents; later reads of the same
resolved path, including reads from resident functions, reuse that value. A
file edit becomes visible only after a new load and compilation. This keeps
evaluation referentially transparent within the compilation environment.

The evaluator uses that invariant directly. Compound values have shared
immutable storage, so ordinary binding and argument passing retain references
to one allocation rather than recursively copying data. Completed pure calls
are memoized by function identity and the identities of their immutable
arguments. The cache is bounded and may evict entries; eviction affects only
work performed, never the artifact. Neither storage identity nor memoization is
observable or expressible in MAG.

The evaluator still enforces expression-depth, call-depth, and total-step
limits. They bound genuinely new or diverging work. Reusing a completed call
returns its shared result without charging its body to the step budget again.

In particular, graph nodes are ordinary immutable values. Constructing a node
once and using its binding at several edge boundaries shares the original node
and its stored projection internally. The graph remains only an edge set, and
its complete node set is still derived from edge endpoints. No node list,
reference value, registry, or add-node language surface follows from the
implementation.

## Modules

- `core/types.mag` has module identity `core.types` and is imported with
  `(require "core.types")`.
- Every definition has canonical identity `<module>.<local-name>`.
- Unqualified references inside a module resolve to that module first.
- Imports are transitive and retain their original namespaces.
- A module evaluates once per compilation environment, including diamond
  dependency graphs.
- Two files resolving to one canonical module identity are an error.
- Circular imports are rejected with the dependency cycle.
- All module definitions are public; there is no export/private mechanism.

## Values and Types

Core scalar types are `Unit`, `Bool`, `Int`, `Float`, and `String`. Core
collections are `List<T>` and `Map<K, V>`. `Unit` has exactly one value and
does not represent absence; optionality is a library-defined tagged sum.

Named type declarations are nominal. Records and tagged sums compose types;
generic parameters are explicitly bound. Structurally equal declarations in
different modules remain different types. There are no aliases, subtyping,
variance, higher-kinded types, or implicit coercions.

The initial concrete forms are:

```lisp
(type Finding {:path String :line Int :message String})
(type Either [E T] (| {:Invalid E} {:Valid T}))
(type-tag Finding)
(foreign nefor.factory.llm
  {:params LlmParams :input ProviderInput :output (ToolCalls | FinalAnswer)})
(artifact "nefor.graph-modification/v1" lowered-data)
```

`(type-tag T)` is the only way to construct a `TypeTag<T>` witness. The
compiler verifies `T` and stores its canonical qualified identity; library
code can therefore carry or serialize a semantic type without accepting a
string that could name an undeclared type. Named values themselves require an
explicit `(as T value)` refinement.

Foreign identities use `nefor.factory.<name>`. The Nefor artifact format is
`nefor.graph-modification/v1`.

`A | B` is a tagged one-of/union. `A + B` is an all-of product. Runtime firing
semantics for products remain authoritative. Fanout is topology, not a type
operator.

## Libraries

Libraries define ordinary types, constructors, typed functions, validators,
lowerers, and foreign declarations. Higher-level abstractions including
`Either`, `Validated`, graph construction, actor templates, agent loops, shell
helpers, terminal selection, and initial-message policy are library code.

Library functions may hide internal nominal conversions, but the compiler
never invents an implicit coercion.

## Foreign Declarations

A foreign declaration produces a first-class `Foreign<P, I, O>` value. Generic
declarations are specialized explicitly before use. `P`, `I`, and `O` are the
library's semantic MAG contract; the runtime does not claim to know those MAG
types. Compilation validates library calls against that value. Before lowering,
the Nefor library compares the qualified identity and concrete wire tags with
the immutable runtime registry snapshot. The runtime then binds the identity
to an implementation and defensively validates the artifact again.

Runtime registry declarations enter compilation as immutable input data. They
are not hardcoded into the evaluator.

## Nefor Artifact Library

The shipped Nefor library constructs a graph-modification artifact containing:

- actor specifications and qualified foreign identities;
- typed routes;
- explicit initial messages;
- rule bindings;
- result boundary metadata.

The runtime preserves current graph-modification semantics: atomic fold,
sender-bound product slots, typed routing, `Unit` completion, synthesized
failures, monotone actor lifecycle, per-run contexts, and defensive validation.

## Forbidden Compiler Knowledge

Production code in `crates/nefor-mag` must not contain concrete knowledge of
`agent`, `llm`, `adapter`, `run-tool`, `tool-result`, `bash`, `sink`, provider
or tool wire types, task envelopes, graph reachability, or Nefor activation
policy. Graph-specific validation and lowering belong to the shipped library.

## Compatibility

This is an atomic replacement. Old MAG source, examples, compiler builtins, and
artifact formats are removed rather than supported through shims or dual paths.

## Acceptance

- Namespaced direct, transitive, and diamond imports resolve deterministically.
- Nominal records, tagged sums, products, collections, and generic functions
  type-check with precise diagnostics.
- A library-defined `Validated` implementation requires no compiler support.
- A library-defined agent can expose `CodeAudit` without asserting that its
  internal foreign actor emits an undeclared runtime type.
- Unknown foreign identities or wire-incompatible foreign uses fail before
  artifact emission; semantic `Foreign<P, I, O>` mappings remain library-owned.
- The complete Nefor lead/tool loop executes through the new libraries.
- The original `mag.CodeAudit` invalid-route failure cannot reach the kernel.
- Workspace formatting, lint, unit, integration, and starter smoke tests pass.
