# Patterns — canonical shapes for MAG programs

Every pattern here has a clean expression in routes, input contracts, or
rules. If a program needs one of these behaviors, use the listed shape —
inventing a workaround (sentinel messages, polling actors, hand-rolled wait
nodes) means the type system can no longer see what the program does.

This is the canonical catalog; a lead-facing distillation ships with the
stdlib so these patterns are available inside session workspaces.

## Ordering without data (dependency edge)

"A must not start before C finishes", where A does not consume C's output.

**Shape:** the edge `C -> A` carrying `mag.Unit`. A's input contract gains a
`+ mag.Unit` component. The kernel emits the Unit when C completes — C's
factory never knows the edge exists.

**Not:** a flag in params, a custom "done" message from C's prompt, or a
polling actor. Dependency is an edge in the one language, informationless by
type.

## All-of join

"Fire when both A and B have delivered."

**Shape:** the join actor declares a product input `(FindingsA + FindingsB)`.
Slots bind to sender edges, so even `(Findings + Findings)` from two
explorers — or `(Unit + Unit)` from two dependencies — never conflate.

**Not:** an accumulator actor with counting logic in its prompt or params.
Firing is the contract; the kernel assembles the activation.

## Any-of / first delivery wins

"Proceed on whichever source answers first."

**Shape:** union input `(A | B)` — whichever arrives activates alone.

## Race and kill

"Try N approaches concurrently; first to finish wins."

**Shape:** spawn N constellations on the same task; bind a rule on each
finisher whose function returns `{kills: [<the others>]}`. First-applied
wins; the losers' in-flight outputs are void; duplicate kills are logged
no-ops. No coordination logic anywhere in the actors.

## Cycle (the agentic loop)

Cycles are legal as-is. Every cycle exits through a typed variant — the llm's
union output routes `FinalAnswer` out of the loop — so no bound is required or
injected. To budget a loop, opt in: thread a `loop-counter` actor into the
cycle (the agent template does this when `:max-steps` is authored). Its
exhaustion exit is a typed variant (`mag.LoopExhausted`), routed like any
type, usually to a summarizer that turns partial work into a `FinalAnswer`.
Never encode "give up after N tries" in a prompt.

## Fire on failure / repair

"If the build fails, route the evidence to a fixer."

**Shape:** failures are typed outputs — computed failures returned by the
factory, suffered failures (provider error, kill, budget) synthesized by the
kernel. Route the failure type to the repair actor; compose produce → check →
repair with a loop-counter for bounded retry.

**Not:** parsing error text out of a success-shaped output.

## Dynamic fanout (for-each)

"One explorer yields N findings; one agent per finding."

**Shape:** a rule `{on: explorer, fn: fan-out}` whose function maps the
findings to a modification with N namespaced constellations
(`explorer.sub.0.*`, …) and a product-input join for their results. Requires
rule evaluation (post-MVP); until then, fixed-width fanout is static routes.

## Same type, two meanings

"Send X to A on the happy path, X to B as a fallback."

Routes are keyed by type, so two destinations for "different reasons" need
the reason in the type: wrap as distinct types (`Approved<X> | Rejected<X>`)
and route each variant. The distinction becomes visible to validation
instead of living in a prompt convention.

## Absence and timeouts

There is no "fire when X did _not_ happen." Express absence positively: a
timeout is an actor whose failure output routes to the fallback; a missed
precondition is a failure route. Negative predicates over graph state do not
exist by design.
