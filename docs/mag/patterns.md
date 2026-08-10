# MAG patterns

> **Unreleased after v0.4.0** — documents the public MAG surface at `505a764`.

These patterns keep workflow meaning visible to MAG's type and graph validation. Start with the complete program in [Authoring reference](authoring-reference.md#a-complete-graph), then compose the shapes below. The session workspace also contains the current canonical cookbook at `lib/patterns.md`.

## Parallel siblings and a join

Create one source/agent pair per independent assignment and connect every agent to a summarizer whose input is the matching product type. Product slots are bound to sender edges, so repeated types are safe:

```lisp
(type Finding {:summary String})
(type Combined (+ Finding Finding))
```

Two `Finding` producers exactly fill `Combined`; one underfills it and three overfill it. Use one flat edge list. Do not ask a summarizer prompt to count arrivals—the product contract owns that condition.

Use a union instead when the first compatible delivery should proceed:

```lisp
(type FirstResult (| LocalFinding RemoteFinding))
```

## Ordering without data

When `verify` must wait for `build` but does not consume its report, use a `Unit` dependency edge and include `Unit` in the consumer's product input. Do not encode ordering in prompt prose, flags, polling, or sentinel strings.

## Fan-out

Connect one producer to several consumers with several ordinary edges. Fan-out is graph structure; no broadcaster node is needed. All consumers receive the same semantic result and remain independently routable toward the output.

## Typed failure and correction

Every structured agent emits `O | AgentError`. Three useful shapes are:

1. **Terminal union** — connect the full union to the output when the caller should receive either outcome.
2. **Recovery chain** — give a reviewer or fixer the full union so it can use `last_output` and the typed reason.
3. **Separate resident rules** — subscribe to the `O` and `AgentError` arms with `nefor.actors.result-arm`, then return a typed delta for each branch.

Do not parse error-looking prose from `O`. When two values have the same representation but different routing meaning, introduce distinct nominal types such as `Approved` and `Rejected`.

Correction budgets belong to structured output validation: `:max-corrections 2` permits two repair attempts after the initial response. A workflow-level retry uses `nefor.actors.retry-gate` and explicit `Continue<T>` / `Exhausted<T>` branches. Keep these two mechanisms conceptually separate.

## Approval before effects

There are two approval layers:

- **Lead plan review** happens before launching a write-capable graph. Compile, inspect the preview, call `write-review`, then execute after approval.
- **In-graph human judgment** uses `nefor.actors.approval-gate` and routes `Approved` and `Rejected` as domain outcomes.

Neither layer silently grants arbitrary tool access. Agents still have an explicit tool allowlist and runtime policy checks.

## Shell pipeline with a timeout

```lisp
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")
(require "nefor.shell")

(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      search (nefor.shell.command-with-options
               "search" "rg -n TODO src/"
               (as nefor.shell.BashOptions
                 {:timeout_ms (nefor.contracts.timeout-ms 30000)}))
      sort (nefor.shell.pipe-command "sort" "sort")
      result (nefor.graph.output-for "result" sort)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start search)
         (nefor.graph.edge search sort)
         (nefor.graph.edge sort result)]))))
```

Use `mag-eval` for the single `search` node alone. Use the graph for the pipeline because it makes transfer and the terminal result explicit.

## Dynamic planner expansion

Use a resident program only when a planner's runtime `List<Task>` determines the worker count:

1. Put the planner, summarizer, and single output in the initial topology.
2. Subscribe separately to the planner's typed success and `AgentError` arms.
3. Index tasks to derive deterministic worker ids.
4. Build one atomic delta containing workers, typed task messages, routes to a `nefor.dynamic.collector`, and the collector route to the static summarizer.
5. Pass the exact expected sender ids to the collector. It emits once, in that order, and rejects unexpected or duplicate senders.
6. For an empty task list, bypass collector construction with `nefor.dynamic.empty-to`; a zero-input collector cannot fire.
7. Route planner failure directly to the static result and spawn nothing.

The runnable reference is `starter/agentic-loop/dynamic-tasks.mag`. Dynamic rules remain pure environment-side functions; workers cannot spawn workers or mutate a live graph.

## Cycles

Cycles are ordinary edges. Ensure the cycle has a typed exit to the output and remains source-reachable. Agent loops do not have an implicit iteration budget; stop a stuck run through `terminate-graph`. Do not hide a retry limit in prompt wording.

## Worktree isolation

Use `nefor.worktree.create` when a graph owns fresh isolated implementation work, or `nefor.worktree.open` when reuse was decided outside the graph and must be validated. Give explicit absolute paths and route the typed `Worktree` value to the worker. Never make `create` adopt an existing directory, and do not imply that run completion removes or integrates the worktree.
