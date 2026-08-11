# MAG errors and recovery

MAG failures occur at different boundaries. Diagnose the boundary first; changing prompts cannot repair a malformed graph, and recompiling cannot make a hung process exit.

See [Authoring reference](language.md) for valid forms and [Orchestrating MAG](orchestrating.md) for run control.

## Compile-time language errors

Parsing, name resolution, and type checking happen before execution. Typical causes:

- using removed `import` or bare helper syntax instead of literal `(require "...")` and qualified names;
- referencing an undeclared semantic type;
- passing a value that does not conform to a declared record;
- connecting incompatible semantic types;
- giving a product input too few or too many incoming occurrences;
- returning something other than the expected artifact.

Fix the source and compile again. The standalone compiler exits nonzero, writes a human summary to stderr, and emits a structured JSON diagnostic with `code`, `stage`, `message`, and optional `path` on stdout.

## Graph validation errors

A program may type-check as code but return an invalid concrete topology. Compilation applies the graph function and validates the result before any run starts.

### Boundary structure

- Only a real `source<T>` may have zero incoming edges.
- Exactly one real `output<T>` must exist.
- The output is terminal and cannot source another edge.
- Every ordinary node must be source-reachable and able to reach the output.

Setting a role-like string on an ordinary node does not create source or output authority. Use public constructors.

### Node identity

One node id must denote one immutable definition. Reusing an id with different configuration, ports, or behavior is an error even if duplicate edges would otherwise collapse. Give distinct nodes distinct ids, or reuse the exact same node value.

### Product coverage

For input `A + B`, incoming edge types must exactly cover every occurrence. `T + T` needs two sender edges; one underfills it and three overfill it. If the workflow means “either,” use `A | B`. If it means ordering only, use `Unit` as a product component.

### Union coverage

Every possible output alternative needs an ordinary route, a typed resident subscription, or the terminal output path. An agent's result is `O | AgentError`; routing only `O` leaves an uncovered error arm. Connect the whole union or handle both arms.

### Forged values

Do not construct low-level graph or boundary records by copying fields. Compiler-packed values and boundary authority cannot be forged with maps. Use the public node constructors and composition functions.

## Structured-agent errors

A structured agent's declared result is `O | nefor.contracts.AgentError`.

- `ProviderError` reports provider failure and optional detail.
- `OutputValidationError` reports one or more path-specific schema violations.
- `last_output` retains the latest raw model output for diagnosis or a recovery agent.

The agent requests correction up to `:max-corrections`. When the budget is exhausted, `AgentError` is emitted as an ordinary typed result. Route it to the output, a reviewer/fixer that accepts the union, or a resident error rule. Do not claim `O` was produced and do not parse provider prose as a substitute.

## Shell failures and hangs

Process and shell nodes are unbounded when their required timeout record uses `no-timeout`. A process that never exits keeps its run nonterminal, so `await-run` also waits indefinitely. Use `timeout-ms` in the `process.exec` or `shell.script` parameter record when an operation needs a wall-clock bound. Never launch a persistent foreground server or watcher as a normal awaited run.

A compile success proves the command node is well-formed, not that its executable, working directory, permissions, or exit status will succeed. Handle routeable command outcomes where the library exposes them; otherwise an unhandled runtime failure fails the run.

## Approval errors

Compilation and preview are not approval. A write-capable lead graph must pass `write-review` before execution.

- In safe mode, the call waits for `/approve` or `/reject`.
- In autonomous mode, human judgment cannot be fabricated and the review is denied.
- In yolo mode, the gate approves.
- Approval is turn- and session-scoped; new ordinary user input invalidates it.

A rejected plan should be revised and submitted again. A discarded review means the user's reply is fresh input, not authorization. Tool-level risk checks remain independent of plan approval.

## Run-control errors

Run ids are opaque and session-scoped. Common outcomes include:

- **malformed** — the value is not a valid MAG run handle;
- **unknown** — no lead-dispatched run is retained for it;
- **wrong session** — the handle belongs elsewhere;
- **expired** — its retained terminal outcome aged out;
- **unauthorized** — a delegated agent tried to control a run it did not directly dispatch.

Use the exact `run_id` returned by `mag execute` or lead `mag-eval`. Call `await-run` once when dependent work needs completion; use `graph-status` only as a one-shot snapshot. Canceling an await detaches that waiter but leaves the run alive. Use `terminate-graph(run_id)` to request termination, then wait for canonical confirmation rather than assuming the request itself killed the run.

## Dynamic-rule errors

Dynamic expansion is validated before its change is applied. Keep each rule's subscribed port and function result typed, return the proper delta artifact, and make the delta atomic.

For collectors, expected sender ids are part of correctness. Unexpected senders, duplicate delivery, or an incomplete drain fail rather than silently reorder results. Handle an empty worker list with `nefor.dynamic.empty-to`; do not instantiate an empty collector and wait for an activation that cannot happen.

## Worktree errors

`nefor.worktree.create` is fresh-only: an existing path or local branch is an error. `nefor.worktree.open` is validation-only: a mismatched repository, path, or branch is an error and nothing is created. Both require explicit absolute paths.

A successful worktree survives the run. Do not interpret run completion as merge or cleanup, and do not retry `create` as `open` unless reuse was an explicit orchestration decision.

## Correction sequence

1. Identify whether failure occurred during language checking, concrete graph validation, runtime execution, approval, or run control.
2. Name the failed node, type, rule, or handle from the diagnostic.
3. Change the source or plan; do not blindly resubmit the same artifact.
4. Compile and inspect the new preview.
5. Re-obtain approval if the write-capable plan changed or its approval expired.
6. Execute as a fresh run and retain its new `run_id`.
