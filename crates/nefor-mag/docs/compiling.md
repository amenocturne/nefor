# Compiling MAG

See [Language reference](language.md) for the language and [Errors](errors.md) for diagnosis.

The `mag` binary compiles and validates a program without any embedding application:

```sh
mag compile main.mag \
  --source-dir ./mag \
  --module-root ./mag \
  --input settings=./settings.json
```

`main.mag` is resolved beneath `--source-dir`. `.mag` entries use the new syntax and `.magl` entries use the legacy Lisp frontend; `--syntax new|lisp` overrides the entry only, while every imported module keeps the frontend selected by its own suffix. Repeat `--module-root` to add module search roots and `--input NAME=PATH` to expose immutable JSON values through the typed host_input boundary. If no module root is supplied, the source directory is used. Successful Rust compilation APIs return the raw artifact value, and the CLI prints exactly that value as JSON with no compiler-owned envelope or fields. `--profile` keeps stdout unchanged and prints one machine-readable profile object to stderr on success. Compilation failures keep the same diagnostic code, stage, message, path, and syntax detail, exit nonzero, and produce no stdout result. When profiling is enabled and compilation started, the diagnostic also carries the partial profile; failures while validating CLI paths or host inputs occur before compilation and have no profile.

Profiles contain `total_duration_ns`, inclusive phase durations, and deterministic operation counters. Total duration covers the complete compile/load attempt and is recorded on success and failure. A phase is recorded whenever it starts, even if that work returns an error. Durations can be nested: entry and module evaluation include checking and any required-module work they trigger, so phase values are not additive. Rust callers that need a failed attempt's profile supply a `CompileProfiler` through `CompilerSession::compile_with_profiler` or `compile_file_with_profiler`, then snapshot it after receiving the unchanged `MagError`. `module_cache_hits` describes repeated module requests served within that one program's compilation; it does not represent a cache hit shared across compile or load requests.

Standalone compilation emits an artifact but has no execute subcommand. Executing an artifact is the job of the application that embeds MAG.
