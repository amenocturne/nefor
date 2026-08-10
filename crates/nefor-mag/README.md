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

Authoring types are `Unit`, `Bool`, `Int`, `Float`, `String`, `List`, `Map`,
records, named types, sums, and products. `JsonValue` is available only when
heterogeneous JSON is itself the declared domain; maps cannot be cast into it.
Named declarations are nominal, generic parameters are explicit, `|` is a
one-of sum, and `+` is an all-of product. Runtime concepts are introduced by
libraries and qualified `foreign` declarations rather than untyped values.

Functions have explicit signatures. The first form is monomorphic; the second
binds generic types explicitly:

```lisp
(fn [[value Int]] -> String (str value))
(fn [T E] [[value T] [convert (Fn T E)]] -> E (convert value))
```

Use `(as Target value)` to validate and refine a structurally typed value into
a declared nominal or collection type. Host inputs are inferred as concrete
records and collections; there is no universal type or cast escape hatch.

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

Libraries stop artifact construction with `(fail diagnostic-data)`. Raw host
inputs remain compiler-private; libraries consume typed projections such as
`(foreign-contracts)`.

## Compilation snapshot

A loaded MAG program observes immutable compilation inputs. Modules evaluate
once, and the first `(read path)` snapshots that file's raw contents for the
life of the loaded program; every later read of the same resolved path reuses
that snapshot, including resident function evaluation. To observe an edited
file, load and compile the program again. Interpolation remains per call and is
applied after retrieving the cached raw contents.

Compound values are also structurally shared inside the evaluator. Binding,
passing, and storing the same immutable list, map, or typed value keeps a small
reference to its original allocation instead of copying the complete value.
Completed pure function calls on the same function and shared argument values
reuse their result within the loaded program. The memoization cache is bounded;
eviction may cause recomputation but cannot change a MAG result. These are
implementation properties, not reference or cache operations in the language.
The expression-step limit remains a guard against genuinely new or diverging
evaluation; a memoized call does not spend that budget on its body again.

For graph libraries this means a node bound once can be used at any number of
edge boundaries without duplicating its stored definition or reevaluating pure
node projections. Graph membership remains wholly determined by those edge
boundaries; there is still no separate node registry or add-node operation.

## Structural type descriptors

`(type-schema (type-tag T))` reifies a MAG data type into an opaque,
compiler-owned `TypeSchema`. Named types remain named for diagnostics while
their substituted bodies describe the runtime structure. Records are strict:
all fields are required and additional fields are rejected. The descriptor can
represent `JsonValue`, `Unit`, `Bool`, `Int`, `Float`, `String`, lists,
string-keyed maps, records, unions, and products for MAG-side validation.
Functions, foreign capabilities, artifacts, type tags, unresolved variables,
and maps with non-string keys fail while loading the MAG program.

The structured-agent provider boundary deliberately supports a narrower,
faithful projection into OpenAI's strict JSON Schema dialect. Closed records,
nested lists, primitives, and nominal sums are supported; non-record roots use
a closed `value` envelope, and sums use mutually exclusive single-tag `anyOf`
branches. Maps and products use reversible entry-list and positional-object
encodings. `JsonValue` is rejected before provider execution because
unrestricted JSON cannot be expressed faithfully. `Int` requests carry signed
64-bit bounds, while `Float` accepts all finite JSON numbers, including integral
syntax. After inverse projection, the original `TypeSchema` remains
authoritative.

The schema and `(type-evidence ...)` descriptors are transport-neutral opaque
values. They serialize only when the compiler constructs an artifact; named
descriptors include their concrete substituted bodies, and maps cannot forge
them. Descriptor compatibility and graph product coverage are compiler
operations rather than library-side inspection of `kind`/`items`. Nefor's
structured-agent library chooses JSON as one external encoding, while the MAG
compiler itself has no provider, LLM, prompting, or retry behavior.

Semantic type identities use compiler-created witnesses rather than strings:

```lisp
(type Audit {:findings (List String)})
(type-tag Audit) ; (TypeTag main.Audit), serialized as "main.Audit"
```

Unknown types fail at `type-tag`, and nominal values require explicit
refinement with `(as Audit value)`.

## Standalone compiler CLI

`mag compile` exposes the same load-time evaluator and resident-rule checks as
the runtime's `mag.load` path, but has no NCP transport, capability bridge, or
run operation:

```sh
mag compile main.mag \
  --source-dir ./program \
  --module-root ./mag-libs \
  --module-root ./project-libs \
  --registry ./kernel-registry.json
```

`--module-root` and `--registry` are repeatable. With no module root, the source
directory is the sole module root. A `.lua` registry is the same kernel
definition file Nefor loads and must expose `registry_contracts`; loading it
constructs only the immutable registry and cannot begin a run. A JSON registry
is either the `foreign_contracts` array itself or an object containing that
array, matching the immutable snapshot Nefor receives from its MAG kernel.
Multiple files are concatenated in command-line order. The compiler does not
supply a kernel or built-in factory registry; callers must provide the contracts
their program uses.

Stdout is exactly one compact JSON document. Both outcomes use envelope version
`1`:

```json
{"version":1,"ok":true,"artifact":{"format":"...","data":{}},"hash":"..."}
{"version":1,"ok":false,"error":{"code":"type_error","stage":"typecheck","message":"..."}}
```

Compilation failures exit `1`; argument/help handling uses clap's normal exit
codes. Human-readable failure context goes to stderr, so subprocess consumers
can parse stdout without filtering logs. Syntax failures additionally carry the
immutable source snapshot, its display identity/path, canonical half-open UTF-8
byte spans, 1-based Unicode and display columns, excerpt/caret, and an optional
related opener. Tabs advance to four-column stops for display; CRLF is one line
break; EOF is a zero-width span at `source.len()`. Paths use Rust's lossless-when-
possible `Path::display()` policy. Required-module diagnostics identify the
resolved module path and snapshot that module rather than the entry file. The
command only compiles and validates an artifact: there is deliberately no `run`
or `execute` subcommand.

Compiler profiling is exposed by `mag compile --profile`; run `just bench-mag`
for the opt-in optimized benchmark and inspect `benches/mag_compile.rs` for the
current benchmark corpus and measurements.

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

## Language documentation

- [Language and authoring reference](docs/language.md)
- [Compiler errors and recovery](docs/errors.md)
- [Compilation and orchestration](docs/orchestrating.md)

Runtime actor semantics and the concise canonical cookbook belong to the
[`mag` plugin](../../plugins/mag/README.md). Seeded capabilities are best read
from the MAG library source in the active composition, for example
[`examples/nefor-agent/mag/lib`](../../examples/nefor-agent/mag/lib/).
