# MAG

MAG is a pure, namespaced data-construction language. Programs load public
libraries, compose ordinary typed values, and return one host-recognized value:

```lisp
(require "core.validated")

(type Finding {:path String :line Int :message String})
(type Audit {:findings (List Finding)})

(artifact "nefor.graph-modification/v1"
  {:actors [] :routes [] :messages []})
```

`core/validated.mag` has module identity `core.validated`; its definitions are
available under that namespace, including through transitive imports. Modules
are evaluated once per compilation, and circular imports are rejected.

Core types are `Artifact`, `Data`, `Unit`, `Bool`, `Int`, `Float`, `String`, `List`, and
`Map`. `Data` is the closed recursive JSON algebra (scalars, lists, and string-keyed
maps), not a top type. Named declarations are nominal, generic parameters are explicit, `|` is
a one-of sum, and `+` is an all-of product. Runtime concepts are introduced by
libraries and qualified `foreign` declarations rather than compiler builtins.

Functions have explicit signatures. The first form is monomorphic; the second
binds generic types explicitly:

```lisp
(fn [[value Int]] -> String (str value))
(fn [T E] [[value T] [convert (Fn T E)]] -> E (convert value))
```

Use `(as Target value)` to validate and refine untrusted `Data`, such as host
inputs, into a declared nominal or collection type. There are no implicit
coercions from `Data`.

Foreign capabilities are first-class. A generic declaration is specialized
with types before use, and its qualified runtime identity can be projected as
ordinary data:

```lisp
(foreign runtime.worker [P I O] {:params P :input I :output O})
(specialize runtime.worker [WorkerParams Task Result])
(foreign-id (specialize runtime.worker [WorkerParams Task Result]))
```

The specialized value has type `(Foreign WorkerParams Task Result)`, so generic
library constructors can validate parameters and boundaries without knowing
what the capability implements.

Libraries stop artifact construction with `(fail diagnostic-data)`. Immutable
host data is available through the `inputs` map, for example
`(get inputs :foreign_contracts)`.

Semantic type identities use compiler-created witnesses rather than strings:

```lisp
(type Audit {:findings (List String)})
(type-tag Audit) ; (TypeTag main.Audit), serialized as "main.Audit"
```

Unknown types fail at `type-tag`, and nominal values require explicit
refinement with `(as Audit value)`.

The embedding API accepts immutable JSON inputs:

```rust,ignore
let program = nefor_mag::load_with_inputs(root, "main.mag", inputs)?;
let format = program.artifact.format;
let data = program.artifact.data;
```

Entry files and library modules may live under different roots. Embedders pass
an explicit module search path with `load_with_inputs_and_module_roots`; module
identity remains relative to the matched root (`core/types.mag` is always
`core.types`). If the same identity exists in multiple roots, loading fails as
ambiguous instead of selecting one by search order.
