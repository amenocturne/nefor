# Patterns — canonical shapes for MAG programs

## Planner → workers → collector → static result

A typed result selector exposes the planner's `List<Task>` and `AgentError`
ports to separate resident rules. The success rule uses `indexed-map` to pair the actual task list with deterministic
positions, maps those values into real structured-agent fragments, then
returns one atomic delta containing those workers, typed task messages,
worker-to-collector routes, and the collector route to a summarizer already in
the initial graph. The error rule routes the complete `AgentError` to the
static outcome and spawns nothing. For `[]`, `nefor.dynamic.empty-to` targets the summarizer's
typed `Port<List<T>>`; a zero-input collector would never fire. The shipped
program is `starter/agentic-loop/dynamic-tasks.mag` and its real provider E2Es
cover zero, invalid, and reverse worker completion.

Every shipped pattern here has a clean expression in routes or input contracts.
If a program needs one of these behaviors, use the listed shape — inventing a
workaround (sentinel messages, polling actors, hand-rolled wait nodes) means
the type system can no longer see what the program does. Dynamic expansion uses
typed resident MAG rule functions returning `nefor.graph-delta/v1`; actors
still receive no graph authority.

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

## Cycle (the agentic loop)

Cycles are legal as-is and unbounded. Every cycle exits through a typed
variant — the agent's result union routes `O | AgentError` out of the loop — so
the typed exit is the terminator. There is no loop-budget mechanism: a run
that never reaches its exit is stopped via the control plane's
kill/interrupt. Never encode "give up after N tries" in a prompt.

## Fire on failure / repair

"If the build fails, route the evidence to a fixer."

**Shape:** foreign/completion failures are typed outputs when an implementation returns
a failure tag (for example the shell capability's `mag.CommandFailed`). Route that failure
type to the repair actor; compose produce → check → repair as an ordinary cycle.
Unhandled failures escalate to `mag.run_failed`. `kill` removes actors and voids
late outputs; it is not a general routeable failure output.

**Not:** parsing error text out of a success-shaped output.

## Same type, two meanings

"Send X to A on the happy path, X to B as a fallback."

Routes are keyed by type, so two destinations for "different reasons" need
the reason in the type: declare distinct nominal variants (for example
`Approved` and `Rejected` records) and expose their union
and route each variant. The distinction becomes visible to validation
instead of living in a prompt convention.

## Absence and timeouts

There is no "fire when X did _not_ happen." Express absence positively: a
timeout is an actor whose failure output routes to the fallback; a missed
precondition is a failure route. Negative predicates over graph state do not
exist by design.
