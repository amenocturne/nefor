# MAG patterns — library composition

MAG programs import namespaced libraries, construct typed data, validate it,
and return an `Artifact`. Graphs, agents, shell commands, and terminal policy
are library values—not compiler syntax.

## One-off shell work

`mag-eval` accepts a graph-fragment expression and supplies the imports,
initial `Unit` message, graph finish, validation, and artifact wrapper:

```lisp
(nefor.shell.command "list" "ls src")
```

Pipe stdout into another command by composing fragments:

```lisp
(nefor.graph.connect
  (nefor.shell.command "search" "rg -n TODO src/")
  (nefor.shell.pipe-command "sort" "sort"))
```

Use `nefor.shell.command-with-options` for an explicit timeout. Commands still
pass through the capability gate.

## Agent program

Import `nefor.actors`, `nefor.contracts`, `nefor.graph`, and `nefor.artifact`.
Create an agent fragment with `nefor.actors.agent`, add an explicit initial
message, finish the graph, and call `nefor.artifact.compile`. Pass semantic
types with `(type-tag T)`; the compiler rejects unknown types and the library keeps them separate from the
LLM's runtime wire tag.

Compose single-exit fragments with `nefor.graph.connect`. Each fragment owns
human-readable actor ids under its configured prefix. Agent tool lists contain
context I/O plus `mag-eval` for world work; add `edit_file`/`write_file` only to
write-capable agents.

## Result boundary

`nefor.graph.finish` selects the fragment output structurally. There is no sink
node and no implicit terminal. Every useful output must either route onward or
be the selected result port; library validation rejects uncovered outputs.

## Products, unions, and cycles

- `A + B` is an all-of product. Runtime slots bind to sender edges, so equal
  component types from different actors do not conflate.
- `A | B` is a one-of union. Either arriving variant can satisfy the input.
- Cycles are ordinary routes. A typed output leaving the cycle is its exit;
  runaway work is stopped through the control plane.
- Completion ordering uses explicit `Unit` messages/routes. Absence is modeled
  positively through a timeout or failure output, never a negative predicate.

## Semantic distinctions

Routing uses runtime wire tags, while library composition checks semantic MAG
types. When equal payload shapes carry different meanings, declare distinct
nominal types and expose them at fragment boundaries. Do not encode routing
meaning only in prompts or parse error text from success-shaped values.
