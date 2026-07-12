# MAG patterns — canonical authoring contract

MAG programs load namespaced libraries with the literal string form
`(require "library.name")`. A module such as `nefor.actors` then exposes
qualified symbols such as `nefor.actors.agent` and
`nefor.actors.AgentConfig`.

Never write `(import ...)`: MAG has no `import` form. Never use the removed
bare helpers `agent`, `node`, or `graph`. Do not copy syntax from historical
session files under the session data directory; those files may have been
written for an older dialect. This document and successful compilation by the
current MAG compiler are the syntax authority.

## Complete minimal agent program

This is the complete current shape. Keep the `require` forms, construct the
configuration as `nefor.actors.AgentConfig`, pass semantic type witnesses and
runtime wire names separately, provide the initial message, select the result
with `nefor.graph.finish`, and return a compiled artifact.

```lisp
(require "nefor.actors")
(require "nefor.artifact")
(require "nefor.contracts")
(require "nefor.graph")

(let [worker
      (nefor.actors.agent
        (as nefor.actors.AgentConfig {:id "worker"
         :model nil
         :profile "standard"
         :provider "chatgpt"
         :system "Answer the task."
         :tools ["read_file" "mag-eval"]
         :da-policy nil})
        (type-tag nefor.contracts.Task)
        "task"
        (type-tag nefor.contracts.FinalAnswer))
      initial
      (nefor.graph.message
        "worker.entry"
        (as Data {:kind "task" :prompt "Inspect the repository."}))
      program
      (nefor.graph.finish
        worker
        (as (List nefor.graph.Message) [initial])
        (as (List nefor.graph.Rule) []))]
  (nefor.artifact.compile program))
```

Agent tools contain context I/O plus `mag-eval` for world work; add
`edit_file` or `write_file` only to write-capable agents. `:model` and
`:profile` may be `nil` when the runtime supplies them. The runtime inventory
is the authority for available providers, profiles, tools, and foreign actors.

## One-off shell work

`mag-eval` accepts a graph-fragment expression and supplies the required
libraries, initial `Unit` message, graph finish, validation, and artifact
wrapper:

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
