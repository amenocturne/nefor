# MAG patterns — the shapes to reach for

Authoring reference for composing `.mag` programs. Each behavior below has one
canonical shape. Use it; inventing a workaround (sentinel messages, a "done"
line in a prompt, a polling actor, parsing error text) hides what the program
does from the type checker and the validator.

Types are the language. Routing is keyed by output type; firing is keyed by
input type. If two things need to go different places, give them different
types — don't encode the reason in a prompt.

## Templates first

Reach for a template before hand-wiring a constellation:

- `(agent {:id N :model … :provider … :tools […]} : IN -> generic-provider.FinalAnswer)`
  — the tool-use loop (llm ⇄ run-tool). Unbounded: the typed final answer is
  the terminator; a runaway run is stopped via interrupt/kill. Builtin.
- `((get tpl "gate") {:id N :model … :provider …})`
  — produce / human approval / revise on reject. Library (`require "lib/templates"`).

Every template namespaces its internals under `:id`; two instances never
collide, and a shared `:id` is a load-time error. Wire them like nodes:
`(graph entry -> agent  agent -> out  :terminal out)`.

## Shell pipes — MAG as shell

`->` is the pipe. `(bash "cmd")` is a capability node usable inline — no
`let` binding, no ceremony — and a chain of them composes like a pipeline:
the first node's stdout becomes the next node's stdin.

- `(bash "ls src")` — a bare expression is a whole program: the implicit
  terminal carries its stdout out as the run result.
- `((bash "rg -n TODO src/") -> (bash "sort"))` — an infix chain is a
  subgraph; it compiles alone or composes inside a larger graph, and its
  edges are type-checked like any others.
- A command's non-zero exit is a routable failure (`mag.CommandFailed`);
  unrouted, the run fails loudly. Route it to a repair actor when failure
  is part of the design, exactly as in "Fire on failure" below.

Shell nodes go through the same capability gate as agent tool calls —
piping does not bypass command policy.

## Agent tool surfaces — context I/O vs world work

An agent's `:tools` carries context I/O — tools that pull content into the
agent's context or author from it (`read_file`, `read_image`, `edit_file`,
`write_file`) — plus `mag-eval` for world work. World queries whose outputs
are data (listing, searching, running commands) are `mag-eval` shell
expressions, not bespoke tools: `:tools ["read_file" "mag-eval"]` is the
read-only investigator; add the write pair for a builder. Don't reach for
per-query tools; reach for an expression.

## Ordering without data — dependency edge

"A must not start before C finishes", where A doesn't consume C's output.

Edge `C -> A` carrying `mag.Unit`; A's input contract gains `+ mag.Unit`. The
kernel emits the Unit when C completes. Not a flag, not a "done" message.

## All-of join — product input

"Fire when both A and B have delivered."

The join actor declares a product input `(FindingsA + FindingsB)`. Slots bind to
sender edges, so `(Findings + Findings)` from two explorers never conflate. Not
an accumulator with counting logic.

## Any-of / first delivery — union input

"Proceed on whichever answers first." Union input `(A | B)`; whichever arrives
fires alone.

## Race and kill

"Try N approaches; first done wins." Spawn N constellations on the same task;
bind a rule on each finisher returning `{kills: [<the others>]}`. First-applied
wins, losers' outputs go void, duplicate kills are no-ops. No coordination logic
in the actors. (Needs rules — post-MVP.)

## Cycle

Cycles are legal as-is and unbounded; every cycle exits through a typed
variant (the llm's `FinalAnswer` route out of the loop), so the typed exit is
the terminator. There is no loop-budget mechanism — a run that never reaches
its exit is stopped via interrupt/kill. Never "give up after N" in a prompt.

## Fire on failure / repair

"If it fails, route the evidence to a fixer." Failures are typed outputs —
computed failures returned by the factory, suffered failures (provider error,
kill, budget) synthesized by the kernel. Route the failure type to the repair
actor; compose produce → check → repair as an ordinary cycle. Not parsing
error text out of a success-shaped output.

## Dynamic fanout — for-each

"One explorer yields N findings; one agent per finding." A rule
`{on: explorer, fn: fan-out}` maps findings to a modification with N namespaced
constellations plus a product-input join. Needs rules (post-MVP); until then,
fixed-width fanout is static routes.

## Same type, two meanings

Routes are keyed by type — two destinations "for different reasons" need the
reason in the type. Wrap as distinct types (`Approved<X> | Rejected<X>`) and
route each variant, so the distinction is visible to validation.

## Absence and timeouts

There is no "fire when X did _not_ happen." Express absence positively: a
timeout is an actor whose failure output routes to the fallback; a missed
precondition is a failure route. Negative predicates over graph state don't
exist by design.
