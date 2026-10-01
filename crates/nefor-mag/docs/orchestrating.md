# Orchestrating MAG

MAG is Nefor's typed workflow language. Use it when a task needs several agents or commands, parallel work, an approval boundary, review, or a result-dependent stage. Use direct process tools for one bounded command.

This guide describes the lead-facing workflow. See [Authoring reference](language.md) for the language and libraries, [Patterns](../../../plugins/mag/docs/patterns.md) for graph shapes, and [Errors](errors.md) for diagnosis.

## Choose the smallest surface

### Author and launch a program

The lead tool operates on a session MAG workspace:

1. **Write** — `mag-write-file {file,new_string,old_string?}` creates or overwrites a workspace-relative source, or performs one exact unique replacement.
2. **Compile and preview** — `mag-preview {file}` checks an existing program and returns its complete authored node tree. It neither writes nor executes.
3. **Review** — inspect the tree. Before a write-capable graph, submit its concrete plan through `write-review`.
4. **Apply** — after approval, `mag-apply {file}` compiles and applies an existing complete program to a fresh graph. Add `run_id` to apply a Delta to that directly dispatched live graph. Add `content` to write the source file, replacing any existing contents, before compiling and applying it in the same call.

Editing a graph function describes a different future run. It does not retrieve or mutate a running graph.

`write-review` is blocking in safe mode: `/approve` authorizes the plan for the current turn, `/reject` returns a reason, and any other reply discards the pending review and becomes new input. Autonomous mode cannot invent human approval; yolo mode accepts the gate. Approval of the plan does not bypass per-tool risk policy inside agents.

## Workspace lifecycle

Each session gets a writable MAG workspace under its session data. Additional
session-local source modules may live directly in that workspace. Canonical and
configuration-owned libraries remain in their materialized package roots; the
composition supplies those roots explicitly to every compilation. Nothing is
copied into the session.

Paths passed to MAG source tools are normalized paths relative to that workspace. Absolute paths, traversal, and symlink escapes are rejected. Module declarations such as `import nefor.graph.{}` resolve through the configured module roots. Files loaded with `read(...)` or <code>`read_json`(...)</code> are snapshotted on first access by
one compilation; compile again after changing them. The ambient context
names the canonical MAG Book and available module inventory.

## Run lifecycle and control

A fresh `mag-apply` acknowledges asynchronous dispatch with an opaque, stable `run_id` and the complete authored node tree.

- **`mag-await(run_id)`** attaches to that run and blocks until its canonical terminal outcome. Call it once when subsequent work depends on completion. Canceling the waiter does not stop the run.
- **`mag-status()`** is a one-shot snapshot of active runs and recent completed summaries. With a `run_id`, it describes that run. Do not poll it; completion is delivered normally, or use `mag-await`.
- **`mag-terminate(run_id)`** requests termination of exactly one active run. The state remains `terminating` until the runtime confirms the terminal outcome.

Run handles are session-scoped. The root lead can address same-session runs; a delegated agent can await or control only runs it directly dispatched. Recent terminal outcomes are retained only for a bounded window, so an old handle can expire.

## Standalone compilation

The `mag` binary validates a program without starting Nefor:

```sh
mag compile workflow.mag \
  --source-dir ./mag \
  --module-root ./mag \
  --input factory_contracts=./factory-contracts.json
```

`workflow.mag` is resolved beneath `--source-dir`. `.mag` entries use the new syntax and `.magl` entries use the legacy Lisp frontend; `--syntax new|lisp` overrides the entry only, while every imported module keeps the frontend selected by its own suffix. Repeat `--module-root` to add module search roots and `--input NAME=PATH` to expose immutable JSON values through the typed host_input boundary. If no module root is supplied, the source directory is used. Successful Rust compilation APIs return the raw artifact value, and the CLI prints exactly that value as JSON with no compiler-owned envelope or fields. `--profile` keeps stdout unchanged and prints one machine-readable profile object to stderr on success. Compilation failures keep the same diagnostic code, stage, message, path, and syntax detail, exit nonzero, and produce no stdout result. When profiling is enabled and compilation started, the diagnostic also carries the partial profile; failures while validating CLI paths or host inputs occur before compilation and have no profile.

Profiles contain `total_duration_ns`, inclusive phase durations, and deterministic operation counters. Total duration covers the complete compile/load attempt and is recorded on success and failure. A phase is recorded whenever it starts, even if that work returns an error. Durations can be nested: entry and module evaluation include checking and any required-module work they trigger, so phase values are not additive. Rust callers that need a failed attempt's profile supply a `CompileProfiler` through `CompilerSession::compile_with_profiler` or `compile_file_with_profiler`, then snapshot it after receiving the unchanged `MagError`. `module_cache_hits` describes repeated module requests served within that one program's compilation; it does not represent a cache hit shared across compile or load requests.

Standalone compilation emits an artifact but has no execute subcommand. Runtime execution belongs to Nefor's lead workflow because it needs the configured providers, tools, approval policy, session, and run control.

## Orchestration checklist

1. Put every predictable stage—implementation, review, verification, and correction routing—before the single graph output.
2. Use sibling nodes for independent work and typed dependencies for real ordering.
3. Compile and inspect the node tree with `mag-preview`.
4. Obtain `write-review` approval before a write-capable application.
5. Apply, retain the returned `run_id`, and await only when needed.
6. Report completion only to the extent established by the terminal result and checks.
