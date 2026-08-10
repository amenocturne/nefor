# MAG graph cookbook

MAG programs load namespaced libraries with `(require "library.name")` and
compose immutable values. Vectors are list literals; collection functions use
prefix form: `(map function values)` and `(concat left right)`.
Never write `(import ...)`; MAG modules use literal `(require "...")` forms.
Each file used by `(read ...)` is snapshotted on first access for the loaded
program. After editing that file, compile or reload the program to observe it.

## Graph model

A graph is a semantic set of typed edges. Its deterministic list backing is an
implementation detail. Edge endpoints carry their node definitions, so an edge
introduces both missing nodes and reuses equal definitions with the same id.
There is no add-node operation.

```text
(nefor.graph.graph edges)
(nefor.graph.add-edges topology added)
(nefor.graph.remove-edges topology removed)
```

All three are total pure functions. Adding an existing edge and removing an
absent edge return the same graph. Duplicate edges collapse. `replace`,
`update`, `fork`, and `join` are not graph primitives.

Graph functions construct definitions; they do not mutate a running actor
constellation. An authored program is a `Graph -> Graph` function.
`nefor.artifact.compile` applies it to `empty-graph`, validates the complete
result, and produces the artifact for a fresh run from that result. Edit or
compose the function to describe a different future run; there is no stored
graph retrieval or hot graph mutation in this API.

## Complete agent graph

```lisp
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")

(let [start
      (nefor.actors.task-source "task" "Inspect the repository.")
      worker
      (nefor.actors.agent
        (as nefor.actors.AgentConfig {:id "worker"
         :model (nefor.contracts.no-identifier)
         :profile (nefor.contracts.identifier "standard") :provider "chatgpt"
         :system "Answer the task." :tools nefor.actors.general-tools ;; includes "mag-eval"
         :da-policy (nefor.contracts.no-da-policy) :max-corrections 2})
        (type-tag nefor.contracts.Task)
        "task"
        (type-tag nefor.contracts.FinalAnswer))
      result
      (nefor.graph.output "result"
        (type-tag (| nefor.contracts.FinalAnswer nefor.contracts.AgentError)))
      topology
      (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
        (nefor.graph.add-edges graph
          [(nefor.graph.edge start worker)
           (nefor.graph.edge worker result)]))]
  (nefor.artifact.compile topology))
```

`:tools` is the agent's capability boundary. Use
`nefor.actors.read-only-tools` for investigation-only agents,
`nefor.actors.general-tools` for common implementation agents, or author an
explicit string list for a narrower/custom surface. The runtime gate rejects a
model-emitted tool call that is not present in that invocation's allowlist,
even when the tool is globally registered.

`source<T>` is the only node kind allowed to have no incoming edge. It captures
an initial value validated against its specialized `T`, then emits `T`.
`output<T>` is a real `T -> T` identity node;
exactly one must exist and it must be terminal. Every ordinary node must be
reachable from a source and able to reach the output.

Every agent has type `I -> (O | nefor.contracts.AgentError)`. Connect that whole
union to a compatible node. Direct edges dispatch selected constructors to
compatible inputs; resident programs use `nefor.actors.result-arm` for typed
rule subscriptions on the stable result wire. `:max-corrections 0` means the
initial attempt only.

## Composing edge families

`graph` accepts one flat `List<Edge>`. Use ordinary list composition rather
than implicit flattening:

```lisp
(nefor.graph.graph
  (concat
    [(nefor.graph.edge octopus-task octopus)
     (nefor.graph.edge lighthouse-task lighthouse)]
    (concat
      (map (fn [[worker WorkerNode]] -> nefor.graph.Edge
             (nefor.graph.edge worker summarizer))
           workers)
      [(nefor.graph.edge summarizer result)])))
```

The concrete element type in real programs is the relevant nominal node type;
the example names it `WorkerNode` only to show the required function signature.

## Shell work

A one-off `mag-eval` expression returns one node and carries a required 1–5-word
`intent` describing the operation:

```lisp
(nefor.shell.command "search" "rg -n TODO src/")
```

For a pipeline, write a graph program with source, command nodes, and output:

```lisp
(let [start (nefor.graph.source "start" (type-tag Unit) nil)
      search (nefor.shell.command "search" "rg -n TODO src/")
      sort (nefor.shell.pipe-command "sort" "sort")
      result (nefor.graph.output-for "result" sort)]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start search)
         (nefor.graph.edge search sort)
         (nefor.graph.edge sort result)]))))
```

Shell commands are unbounded unless `nefor.shell.command-with-options` receives
`(as nefor.shell.BashOptions
  {:timeout_ms (nefor.contracts.timeout-ms 30000)})`.

```lisp
(nefor.shell.command-with-options
  "bounded-search"
  "rg -n TODO src/"
  (as nefor.shell.BashOptions
    {:timeout_ms (nefor.contracts.timeout-ms 30000)}))
```

## Products, unions, fan-out, and cycles

- `A + B` is an all-of product. Before application, incoming edge types must
  exactly cover its component multiset. Slots bind to sender edges, including
  repeated component types from different nodes.
- `A | B` is a one-of union. Either arriving variant can fire the consumer.
- Fan-out is multiple edges with one producer; fan-in is multiple edges with
  one consumer. Neither needs a special node.
- Cycles are ordinary edges and remain legal when every participating node is
  source-reachable and can reach the output.
- Ordering without data uses a `Unit` edge.
- Absence is represented positively through timeout/failure output, never a
  negative graph predicate.

## Validation boundary

MAG parsing and type checking establish that graph transformations are valid
code. For each run, the transformation is applied to `empty-graph`, then the
concrete returned graph is validated before application: node identity
consistency, foreign contracts, edge types, source/output structure, and
reachability. A rejected transformation starts no run.

Resident rule subscriptions are program metadata, not edges. Their operational
delta artifact remains a separate kernel facility and does not grant actors
authority to mutate graph definitions.
