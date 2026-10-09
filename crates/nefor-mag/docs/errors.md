# MAG errors and recovery

MAG failures occur at different boundaries. Diagnose the boundary first; changing prompts cannot repair a malformed graph, and recompiling cannot make a hung process exit.

See [Authoring reference](language.md) for valid forms and [Compiling MAG](compiling.md) for the standalone compiler.

## Compile-time language errors

Parsing, name resolution, and type checking happen before execution. Typical causes:

- using a Lisp `(require "...")` form in a `.mag` file instead of a supported direct `import module.identity`, `import module.identity.{...}`, or `import module.identity as alias` declaration;
- referencing an undeclared semantic type;
- passing a value that does not conform to a declared record;
- connecting incompatible semantic types;
- giving a product input too few or too many incoming occurrences;
- returning something other than the expected artifact.

Fix the source and compile again. The standalone compiler exits nonzero, writes the structured diagnostic to stderr, and produces no stdout artifact. The diagnostic retains its `code`, `stage`, `message`, optional `path`, and syntax detail.

## Graph validation errors

A program may type-check as code but return an invalid concrete topology. Compilation applies the graph function and validates the result before any run starts.

### Boundary structure

- Only nodes whose complete input type is exactly `Unit` may have zero incoming edges. An ADT or product that merely contains `Unit` still requires an explicit input. An unfed exact-`Unit` root receives one automatic activation; an incoming route or explicit input message suppresses it.
- Exactly one real `output<T>` must exist.
- The output is terminal and cannot source another edge.
- Every ordinary node must be root-reachable and able to reach the output.

Setting a role-like string on an ordinary node does not create root eligibility or output authority. Use public constructors. Fresh-graph activation follows exposed node boundaries. Dynamic deltas retain only actors, so they preserve the narrower exact-`Unit` actor bootstrap rule; send an explicit message to start any other unfed delta actor.

Diagnostics identify the offending boundary and semantic type, for example:

```text
root validation failed: {"input":"unfed.nefor.graph.Value","input_type":{"kind":"primitive","name":"String"},"node":"unfed","reason":"zero incoming edges; only inputs that accept Unit can be activated automatically"}
```

### Node identity

One node id must denote one immutable definition. Reusing an id with different configuration, ports, or behavior is an error even if duplicate edges would otherwise collapse. Give distinct nodes distinct ids, or reuse the exact same node value.

### Product coverage

For input `(A, B)`, incoming edge types must exactly cover every occurrence. `(T, T)` needs two sender edges; one underfills it and three overfill it. If the workflow means “either,” declare a nominal ADT with one constructor for each alternative. If it means ordering only, use `Unit` as a product component. Coverage diagnostics include the input boundary and type plus every incoming route's source boundary and type, making missing or extra occurrences visible.

### Nominal ADT routing

A nominal ADT travels as its complete owner value. An agent's output is `core.types.Result<AgentError, O>`; a direct edge to an `O` input is incompatible because it would erase the `Result` constructor. Connect the whole result, bind its successful continuation with `nefor.node.>=>`, or unpack and repack both constructors explicitly.

### ADT construction

Graph compatibility and value construction answer different questions. Nominal ADTs never accept a payload as though it were the owner value: construct the owner explicitly with `Owner.Constructor(payload)` (for example, `Result<String, Int>.Ok(42)`). Graph branches likewise require explicit unpack and lift operations; a route cannot erase an outer constructor merely because its payload type matches the destination. The compiler reports both source and target types for invalid refinements, but does not currently attach a source span to this evaluation-time diagnostic.

### Forged values

Do not construct low-level graph or boundary records by copying fields. Compiler-packed values and boundary authority cannot be forged with maps. Use the public node constructors and composition functions.

## Structured-agent errors

A structured agent's declared result is `core.types.Result<nefor.contracts.AgentError, O>`.

- Its `Error` constructor carries an `AgentError`; within that value, `ProviderError` reports provider failure and optional detail.
- `OutputValidationError` reports one or more path-specific schema violations.
- `last_output` retains the latest raw model output for diagnosis or a recovery agent.

The agent requests correction up to `max_corrections`. When the budget is exhausted, `AgentError` is emitted as an ordinary typed result. Route the complete `Result` to the output or use the result combinators to handle `Ok` and `Error` deliberately. Do not claim `O` was produced and do not parse provider prose as a substitute.

## Shell failures and hangs

Process and shell nodes are unbounded when their authored timeout is `named(Timeout, Unlimited, nil)`. A process that never exits keeps its run nonterminal. Use `named(Timeout, Milliseconds, N)`, `named(Timeout, Seconds, N)`, or `named(Timeout, Minutes, N)` in the `process.exec` or `shell.script` parameter record when an operation needs a wall-clock bound.

A compile success proves the command node is well-formed, not that its executable, working directory, permissions, or exit status will succeed. Handle routeable command outcomes where the library exposes them; otherwise an unhandled runtime failure fails the run.

## Declarative-operation errors

Dynamic expansion is validated before execution and materialized atomically at each trigger. Version 1 admits only `instantiate_delta_template`, with ordered trigger, capture, field, integer-to-decimal-string, and string-concatenation expressions. Actor references, parameter bindings, relocations, routes, and logical paths must resolve within the closed template; an invalid or conflicting materialization fails the run without applying a partial delta.

For `nefor.node.sequence`, expected sender ids are derived from the supplied nodes. Per-sender FIFOs preserve overlapping activation cohorts, and each complete cohort emits in declared order. Unexpected senders or an incomplete drain fail rather than silently reorder results. An empty node list produces `[]` after each input activation.

## Worktree errors

`nefor.worktree.create` is fresh-only: an existing path or local branch is an error. `nefor.worktree.open` is validation-only: a mismatched repository, path, or branch is an error and nothing is created. Both require explicit absolute paths.

A successful worktree survives the run. Do not interpret run completion as merge or cleanup, and do not retry `create` as `open` unless reuse was an explicit orchestration decision.

## Correction sequence

1. Identify whether failure occurred during language checking, concrete graph validation, runtime execution, approval, or run control.
2. Name the failed node, type, operation, or run from the diagnostic.
3. Change the source or plan; do not blindly resubmit the same artifact.
4. Compile and inspect the new preview.
5. Re-obtain approval if the write-capable plan changed or its approval expired.
6. Execute as a fresh run and retain its new `run_id`.
