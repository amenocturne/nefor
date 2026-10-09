# MAG

MAG stands for **Meta-Algebraic Grammar**: a grammar for constructing typed
algebraic descriptions at the meta level, before anything interprets them. MAG
is a strictly typed, pure functional language that compiles an immutable source
snapshot into one artifact.

```sh
mag compile main.mag --source-dir .
```

## Rust API

`CompilerSession` accepts explicit `CompileRequest` values for in-memory entry
source and `FileCompileRequest` values for file-backed entry programs. Both requests
carry the source directory, host inputs, module roots, and compiler options.
The session is cold-only: every call creates independent compiler state and
`CompilerSessionStats` accounts for requests without implying a cache. No
compiled artifact retains an evaluator or callable function handle. The free
`compile_*` and `compile_file_*` functions use the same cold implementation.

`CompileProfiler` records deterministic operation counts plus inclusive
wall-clock phase durations. Evaluation durations include nested checking and
module work, so phases may overlap and must not be summed. `total_duration_ns`
is the complete compile/load attempt on both success and failure. A profiler
passed to `CompilerSession::compile_with_profiler` or `compile_file_with_profiler` can be snapshotted
after an error without changing the returned `MagError`; profiling likewise
does not alter successful artifacts or the CLI's artifact stdout. Both the new
and explicit Lisp frontends record entry/module lexing and parsing; new-syntax
parsing includes lowering into authored IR.

Cold compilation must not copy dynamic values to find nominal declarations.
Declaration lookup borrows visible frame slots and clones only the matching
declaration. Call environments import only declarations from caller frames not
already captured in their lexical environment. Semantic descriptor JSON is
assembled by moving child values, rather than recursively serializing completed
subtrees again at each parent. These are local cost reductions, not additional
cross-compilation caches or changes to descriptor identities.

`module_cache_hits` counts repeated `require` requests served by the module
table of the program currently being compiled. It is deterministic per-program
work accounting, not a hit from a future cache shared across compilations.

## Compiler pipeline

MAG keeps surface syntax separate from language semantics:

```text
selected lexer/parser -> authored module IR -> checked IR -> evaluation
```

The expression-oriented frontend parses nominal declarations, calls, tuples,
blocks, typed lambdas, imports, and binding-scoped infix chains. The explicit
Lisp frontend retains its reader and lowering path. Both lower into the same
authored IR, which keeps unresolved names and explicit declarations, block
items, expressions, and type forms; environment-dependent binding, overload,
generic, and compatibility resolution remains in the checker.

File entry selection is deterministic across the CLI, compiler sessions,
project builds, observation, and plugin loading: `.mag` selects the new syntax
and `.magl` selects Lisp. CLI `--syntax new|lisp` and Rust `*_with_syntax`
functions explicitly override only the entry. In-memory APIs default to the new
syntax. Required modules always use their own suffix, both suffixes for one
module identity are ambiguous, and a syntax error never retries another
frontend. Evaluation stays syntax-independent and MAG core contains no
application-specific behavior.

## Explicit project builds for embedders

`project_config::prepare(project_root, extra_roots)` loads exactly
`<project_root>/mag.toml`, rejects unsupported versions and unknown fields,
and validates the project and effective root directories. It returns
`PreparedProject { project_root, module_roots, config_version }`. Effective
roots are the project root, manifest roots, then extra roots, preserving order
and duplicates; relative roots are joined to the explicit project root. There is
no parent discovery or canonicalization. `ProjectError` exposes the input-stage
`code`, `path`, and `message`; CLI diagnostics preserve these fields.

Pass these roots and freshly materialized host inputs in a `FileCompileRequest`
to `project_cache::build_in(request, config_version, cache_dir, policy, profiler)`
when immutable project source needs separate writable storage. The directory
contains the `v2/compiler/request/record` hierarchy. Its location is
not an identity input: original project root, entry, ordered roots, full host
inputs, options, manifest version, and compiler executable bytes still are.
Embedders should supply absolute project and cache paths. The compiler identity
is the current embedding executable, not a separate CLI binary.

`build` and `build_with_identity` retain root-local `.mag/cache` storage, as does
CLI `mag build`. `build_in_with_identity` supplies the isolated test/benchmark
identity seam for explicit storage. `CachePolicy::Bypass` neither looks up nor
publishes records nor hashes the executable. Successful output bytes remain
exact; `BuildOutput.cache` is diagnostic metadata outside the artifact.
Unavailable identity reports `Unavailable`; an unsuccessful lookup reports
`Miss`, including when later publication fails. Storage failures never prevent
successful cold output. This cache contains compiler successes, not runtime
acceptance: embedders must still validate artifacts against their current runtime.

## Documentation

- [The MAG Book: core language](<../../mag/book/01. core/README.md>)
- [Language reference](docs/language.md)
- [Compiler errors and recovery](docs/errors.md)
- [Compiling MAG](docs/compiling.md)
