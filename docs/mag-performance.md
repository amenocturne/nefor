# MAG compiler profiling and performance

> **Internal design record.** This material records the reasoning and implementation shape of the MAG work at the time it was written. It is not an end-user contract. Current authority is `just bench-mag`, the benchmark implementation, and current compiler tests; where they differ, code and current tests win.

The MAG compiler has two opt-in observability paths. Normal library calls and
the ordinary CLI envelope remain unchanged and do not allocate a profiler.

## Profiling one compile

Pass `--profile` to the standalone compiler:

```sh
mag compile starter/agentic-loop/lead-turn.mag \
  --source-dir . \
  --module-root starter/mag/lib \
  --profile
```

The success envelope gains a `profile` object. Durations are nanoseconds spent
at Rust-observable boundaries: entry read/lex/parse/evaluation, aggregate
module resolution/read/lex/parse/evaluation, function checking, artifact
conversion, artifact serialization plus SHA-256, and resident-rule validation.
Module evaluation contains nested module work, so phase values are not intended
to be summed into an exclusive total. Descriptor construction and graph
validation happen inside MAG evaluation; the compiler cannot truthfully split
them without instrumenting those libraries, so their cost remains evaluation.

Counters are deterministic descriptions of work rather than performance
thresholds: evaluator expression steps, calls, closure/environment snapshots,
module requests/loads/cache hits, immutable file-read activity, memoization,
and resident rules checked. They are suitable for semantic regression checks.
Durations are diagnostic and vary with machine load.

Embedders can use `load_profiled` for a completed snapshot or
`load_with_profiler` plus `validate_loaded_rules_profiled` when rule validation
must appear in the same profile.

## Preserved benchmarks

Run the explicit optimized benchmark target:

```sh
just bench-mag > tmp/mag-benchmark.json
MAG_BENCH_SAMPLES=100 just bench-mag > tmp/mag-benchmark-100.json
```

It is not part of `just test`. The JSON report retains raw unprofiled wall-clock
samples, min/median/mean/p90/p95/max distributions, separately collected median
phase profiles, deterministic counters, artifact hashes, build/toolchain
information, Git state, OS, architecture, and logical CPU count. It covers a
trivial artifact, the shipped lead-turn program, linear graph scaling, product
fan-in scaling, and an expected call-depth-limit failure. Successful samples
assert stable artifacts and counters; the failure case asserts the specific
call-depth budget category. There are deliberately no hardware-dependent
pass/fail thresholds. Each iteration is a fresh compilation, not a claim about
a cold OS filesystem cache. `MAG_BENCH_SAMPLES` must be a positive integer.

## Follow-up cache design

The safe cache unit is an immutable **library-world image**, not a live `Env`
or an independent map per module. It should contain the closed set of resolved
module identities and canonical paths, their transitive require graph and
content digests, every non-module `(read ...)` path/content digest and snapshot,
evaluated exported definitions, contributed foreign-declaration identities,
and a compiler/cache ABI version.

Candidate lookup keys are compiler version/ABI, the ordered canonical module
root set, requested root modules, and a normalized host-input digest (initially
the complete `inputs`, including registry contracts). A candidate is reusable
only after re-resolving every identity with the current zero/unique/ambiguous
rules and confirming content hashes for modules and arbitrary read
dependencies. Root additions/reordering, ambiguity, deletion or symlink
retargeting, transitive edits, registry/host-input changes, read-file changes,
and compiler semantic changes invalidate it.

A hit must create a fresh compilation state, install immutable definitions and
validated read snapshots, and replay foreign identities. It must not reuse the
loading stack, entry definitions, resident-rule evaluations, or memoized call
state. Important hazards are closures retaining `Env::snapshot()` values,
identity-based memoization for `Arc` compound values, duplicate foreign
registration across cached and uncached code, circular/diamond import
semantics, and modules reading arbitrary files such as `nefor/toolsets.json`.
The lower-risk first increment is a canonical-path plus content-hash source/AST
cache; evaluated-library caching should wait for the dependency manifest and
fresh-state rehydration above.
