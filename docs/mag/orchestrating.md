# Orchestrating MAG

> **Unreleased after v0.4.0** — documents the public MAG surface at `505a764`.

MAG is Nefor's typed workflow language. Use it when a task needs several agents or commands, parallel work, an approval boundary, review, or a result-dependent stage. For one bounded shell operation, use `mag-eval` instead.

This guide describes the lead-facing workflow. See [Authoring reference](authoring-reference.md) for the language and libraries, [Patterns](patterns.md) for graph shapes, and [Errors](errors.md) for diagnosis.

## Choose the smallest surface

### `mag-eval`: one node expression

`mag-eval` compiles and launches one node expression without creating a `.mag` file:

```lisp
(nefor.shell.command "find-todos" "rg -n TODO src/")
```

The tool call also requires a short, 1–5-word `intent`. It returns immediately with a stable `run_id`; use `await-run` when the next decision needs its terminal result. It is appropriate for finite, one-off commands. Use a program when work needs multiple nodes, agents, routing, review, or a durable source file.

A shell command runs until its process exits. Do not launch a foreground server or watcher and then await it: the run cannot become terminal. Prefer a retained/background facility, or one bounded command that starts the service, checks it, and tears it down.

### `mag`: author and launch a program

The lead tool operates on a session MAG workspace:

1. **Write** — `action="write"` creates or replaces a workspace-relative `.mag` file.
2. **Compile and preview** — `action="compile"` (the default) checks the program and renders the proposed actors and routes. Compilation does not execute the graph and is not approval for writes.
3. **Review** — inspect the preview. Before a write-capable graph, submit its concrete plan through `write-review`.
4. **Execute** — after approval, `action="execute"` compiles and validates the current file again, launches a fresh run, and returns a `run_id`.

Editing a graph function describes a different future run. It does not retrieve or mutate a running graph.

`write-review` is blocking in safe mode: `/approve` authorizes the plan for the current turn, `/reject` returns a reason, and any other reply discards the pending review and becomes new input. Autonomous mode cannot invent human approval; yolo mode accepts the gate. Approval of the plan does not bypass per-tool risk policy inside agents.

## Workspace lifecycle

Each session gets a writable MAG workspace under its session data. On initialization Nefor copies the configured `mag/lib/` tree into `lib/` **without overwriting existing files**. This gives programs stable, session-local contracts, libraries, prompt fragments, and the canonical `lib/patterns.md` cookbook.

Paths passed to the lead `mag` tool are relative to that workspace. Literal module imports such as `(require "nefor.graph")` resolve through its seeded `lib/`. Files loaded with `(read ...)` are snapshotted on first access by a loaded program; recompile or reload after changing them. Do not copy MAG files out of old session directories as templates: use the current seeded libraries and cookbook.

## Run lifecycle and control

Both `mag action="execute"` and lead-dispatched `mag-eval` acknowledge dispatch with an opaque, stable `run_id`.

- **`await-run(run_id)`** attaches to that run and blocks until its canonical terminal outcome. Call it once when subsequent work depends on completion. Canceling the waiter does not stop the run.
- **`graph-status()`** is a one-shot snapshot of active runs and recent completed summaries. With a `run_id`, it describes that run. Do not poll it; completion is delivered normally, or use `await-run`.
- **`terminate-graph(run_id)`** requests termination of exactly one active run. The state remains `terminating` until the runtime confirms the terminal outcome.

Run handles are session-scoped. The root lead can address same-session runs; a delegated agent can await or control only runs it directly dispatched. Recent terminal outcomes are retained only for a bounded window, so an old handle can expire.

## Standalone compilation

The `mag` binary validates a program without starting Nefor:

```sh
mag compile workflow.mag \
  --source-dir ./mag \
  --module-root ./mag/lib \
  --registry ./foreign-contracts.json
```

`workflow.mag` is resolved beneath `--source-dir`. Repeat `--module-root` to add module search roots and `--registry` to combine Lua or JSON foreign-contract snapshots. If no module root is supplied, the source directory is used. `--profile` adds compiler phase timings and deterministic counters to the JSON success envelope.

Standalone compilation emits an artifact and content hash, but has no execute subcommand. Runtime execution belongs to Nefor's lead workflow because it needs the configured providers, tools, approval policy, session, and run control.

## Orchestration checklist

1. Put every predictable stage—implementation, review, verification, and correction routing—before the single graph output.
2. Use sibling nodes for independent work and typed dependencies for real ordering.
3. Compile and inspect the preview.
4. Obtain `write-review` approval before a write-capable execution.
5. Execute, retain the returned `run_id`, and await only when needed.
6. Report completion only to the extent established by the terminal result and checks.
