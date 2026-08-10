# MAG authoring reference

> **Unreleased after v0.4.0.**

MAG is a small, typed, expression-oriented language. Programs compose immutable graph values from namespaced libraries. This reference covers the supported author-facing layer; runtime implementation structures are intentionally not public API.

See [Orchestrating MAG](orchestrating.md) for lead tools, [Patterns](patterns.md) for common topologies, and [Errors](errors.md) for diagnostics.

## Modules and data

Load modules only with literal requires:

```lisp
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")
```

Do not use historical `import` forms or unqualified helper names. Vectors are list literals, records use maps, and collection functions use prefix form: `(map f values)`, `(concat left right)`, `(fold f initial values)`.

Declare nominal records and algebraic types with `type`:

```lisp
(type Finding {:path String :summary String})
(type Decision (| Finding nefor.contracts.AgentError))
(type Pair (+ Finding Finding))
```

`A | B` is a one-of union. `A + B` is an all-of product. Product occurrences matter: `T + T` requires two matching incoming edges from distinct senders.

## A complete graph

```lisp
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")

(let [start
      (nefor.graph.source "task"
        (type-tag nefor.contracts.Task)
        (as nefor.contracts.Task {:prompt "Inspect the repository."}))
      worker
      (nefor.actors.agent
        (as nefor.actors.AgentConfig
          {:id "worker"
           :model (nefor.contracts.no-identifier)
           :profile (nefor.contracts.no-identifier)
           :provider "chatgpt"
           :system "Inspect the repository and report the result."
           :tools nefor.actors.read-only-tools
           :da-policy (nefor.contracts.no-da-policy)
           :max-corrections 2})
        (type-tag nefor.contracts.Task)
        "task"
        (type-tag nefor.contracts.FinalAnswer))
      result
      (nefor.graph.output "result"
        (type-tag (| nefor.contracts.FinalAnswer
                     nefor.contracts.AgentError)))]
  (nefor.artifact.compile
    (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph
      (nefor.graph.add-edges graph
        [(nefor.graph.edge start worker)
         (nefor.graph.edge worker result)]))))
```

An authored program is a pure `Graph -> Graph` function. `nefor.artifact.compile` applies it to `nefor.graph.empty-graph`, validates the complete topology, and prepares a fresh run. Build one flat edge list; compose edge families with `concat` and `map` rather than nested lists.

### Sources and output

`nefor.graph.source<T>` captures and emits a value checked against `T`. It is the only node allowed to have no incoming edge.

`nefor.graph.output<T>` is a concrete `T -> T` identity node and the result boundary. A graph must contain exactly one, it must be terminal, and every ordinary node must be reachable from a source and able to reach it. `nefor.graph.output-for` derives the compatible type from a preceding node.

### Semantic types and wire names

A port has two distinct pieces of information:

- `(type-tag T)` is compiler-checked semantic evidence used for values and edge compatibility.
- A wire string names the runtime protocol channel expected by the library constructor.

For example, an agent may take semantic `nefor.contracts.Task` on wire `"task"`. Do not substitute a wire string for a type, invent protocol names, or attempt to build lower-level runtime records. Prefer public constructors such as `source`, `agent`, `output`, `command`, and `worktree.create`.

## Agents

`nefor.actors.agent` has semantic type:

```text
I -> (O | nefor.contracts.AgentError)
```

The whole union must be handled by a compatible downstream node or terminal output. `AgentError` preserves `last_output` and classifies the reason as provider failure or structured-output validation failure. The structured agent automatically asks the model to correct invalid output up to `:max-corrections`; `0` means only the initial attempt. Exhaustion emits `AgentError` as data—it is not a successful `O` and should be routed deliberately.

`:tools` is the agent's capability boundary. Use `nefor.actors.read-only-tools`, `nefor.actors.general-tools`, or an explicit list. A tool call not in the invocation allowlist is rejected even if the tool exists globally. `:da-policy` configures command policy; it does not replace the runtime approval gate.

If a downstream reviewer can work with partial failed output, accept the full union as input. Otherwise route success and error separately in a resident program with `nefor.actors.result-arm`.

## Edges, products, unions, and joins

`nefor.graph.edge` connects compatible nodes. A producer may fan out through several edges; a consumer may fan in through several edges.

- A single input fires for each matching arrival.
- A union input `A | B` fires when either variant arrives.
- A product input `A + B` fires only after every product occurrence is filled. This is the all-of join mechanism.
- Multiple edges from one producer express fan-out.
- A `Unit` dependency edge expresses ordering without transferring domain data.
- Cycles are legal if all nodes remain source-reachable and output-reachable.

There is no graph mutation API. `graph`, `add-edges`, and `remove-edges` are total pure set operations over the graph being authored.

## Shell nodes

```lisp
(require "nefor.shell")

(nefor.shell.command "search" "rg -n TODO src/")
```

`command` takes `Unit`; `pipe-command` consumes the preceding text. Commands are unbounded by default. Bound an operation explicitly:

```lisp
(nefor.shell.command-with-options
  "bounded-search"
  "rg -n TODO src/"
  (as nefor.shell.BashOptions
    {:timeout_ms (nefor.contracts.timeout-ms 30000)}))
```

Use `(nefor.contracts.no-timeout)` only when waiting indefinitely is intentional. A timeout or command failure is a runtime outcome, not evidence that compilation failed.

## Human approvals

`nefor.actors.approval-gate` branches a `FinalAnswer` into nominal `Approved` and `Rejected` results. Use it when human judgment is part of the graph's meaning. It is distinct from lead `write-review`, which authorizes execution of a write-capable orchestration plan before launch. See [Orchestrating MAG](orchestrating.md#mag-author-and-launch-a-program).

## Dynamic rules

Most workflows should be fully static. When a runtime result determines how many nodes are needed, use a resident program:

```lisp
(nefor.artifact.compile-program topology rules)
```

A rule subscribes to a typed port created with `nefor.graph.rule`; its named pure MAG function returns a delta artifact. Use public helpers in `nefor.dynamic` for typed worker collection and the empty-list branch. Rules are program metadata, not graph edges, and do not give agents authority to alter the graph. See [Dynamic planner expansion](patterns.md#dynamic-planner-expansion).

## Worktrees

`nefor.worktree.create` and `nefor.worktree.open` are explicit, typed workflow nodes:

- `create` requires absolute repository and worktree paths plus branch and base. It creates only a fresh branch/worktree and refuses to adopt an existing path or local branch.
- `open` validates an existing repository/path/branch triple and never creates or changes it.

Successful worktrees outlive the MAG run. The public capability intentionally has no merge, removal, inventory, or cleanup operation. Route the returned `Worktree` into agents that need the isolated path, and keep integration or cleanup outside the graph unless an explicit capability owns it.
