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

- `(agent {:id N :model … :provider … :tools […] :max-steps N} : IN -> generic-provider.FinalAnswer)`
  — the bounded tool-use loop (llm ⇄ run-tool, loop-counter exit). Builtin.
- `((get tpl "retry-bounded") {:id N :model … :provider … :max-steps N})`
  — produce / typed-failure / repair, bounded. Library (`require "lib/templates"`).
- `((get tpl "gate") {:id N :model … :provider … :max-steps N})`
  — produce / human approval / revise on reject, bounded. Library.

Every template namespaces its internals under `:id`; two instances never
collide, and a shared `:id` is a load-time error. Wire them like nodes:
`(graph entry -> agent  agent -> out  :terminal out)`.

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

## Bounded cycle

Every cycle must pass through a `loop-counter` — load-time enforced. The exit is
a typed variant (`mag.LoopExhausted`), routed like any type, usually to a
summarizer that turns partial work into a `FinalAnswer`. Never "give up after N"
in a prompt. This is the spine of `agent`, `retry-bounded`, and `gate`.

## Fire on failure / repair

"If it fails, route the evidence to a fixer." Failures are typed outputs —
computed failures returned by the factory, suffered failures (provider error,
kill, budget) synthesized by the kernel. Route the failure type to the repair
actor; compose produce → check → repair with a loop-counter. This is exactly
`retry-bounded`. Not parsing error text out of a success-shaped output.

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
