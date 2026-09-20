# Composition semantics for MAG-authored graphs

These are the runtime meanings behind Nefor's node combinators. They are not
workflow recipes: task-specific code chooses whichever composition preserves
the distinctions that matter.

## Fixed and dynamic multiplicity

`nefor.node.sequence` accepts a compile-time `List<Node<I, O>>` and returns
`Node<I, List<O>>`. The supplied node order defines result order even when
actors finish out of order. The list must be nonempty so its node boundary and
result element type come from actual children; an empty list is rejected during compilation.

`nefor.dynamic.DynamicList<O>` is a distinct runtime effect. It emits indexed
occurrences plus explicit completion. Consumers retain that same nominal type;
the operation owns its interpretation. `nefor.dynamic.traverse(id, worker)`
accepts an ordinary typed node and internally instantiates its closed worker
template per occurrence, while
`nefor.dynamic.context` buffers by index and presents one ordered provider turn,
including for zero occurrences. No runtime-sized MAG `List` value or compiler
name-based compatibility privilege exists. The shipped
`examples/nefor-agent/agentic-loop/dynamic-tasks.mag` exercises zero, invalid,
and reverse-completion cases.

## Products and ADTs

A product input such as `(A, B)` fires only after every occurrence arrives.
Slots bind to sender edges, so `(Finding, Finding)` from two producers keeps
the occurrences distinct. `fanout` and `parallel` construct common product
shapes.

A nominal ADT arrives as one complete owner value. `choose` explicitly unpacks an
`Either`, applies one node to each payload, and repacks the selected constructor. When two paths carry the same payload type but different
meanings, distinct nominal types such as `Approved` and `NeedChanges` keep that
reason visible to validation.

The fixed combinators behind these shapes are compile-time transforms —
`Unit`, `Project`, `Pack`, `Unpack`, `Assemble`, and `EmptyList` — carried by
boundaries, routes, and messages. They never become runtime actors. `Assemble`
cohorts belong to the destination and structural path and use one FIFO per
explicit slot, so equal-typed product/list positions retain their authored
meaning.

## Ordering without data

The kernel emits `mag.Unit` when an actor completes successfully. A Unit edge
therefore expresses sequencing without pretending that the downstream node
consumes the upstream result. `nefor.node.*>` is the standard keep-right
composition: it discards the left value, waits for successful completion, and
runs a `Node<Unit, O>`.

## Errors as values

Agent results include `AgentError`; process nodes return `ProcessResult` with a
typed exit-or-signal termination. These are semantic values. A workflow may
route them, retry them, continue with partial evidence, or make them its final
business result.

The bounded retry gate is an ordinary node whose output distinguishes
`Continue T` from `Exhausted T`. Unhandled factory failures instead escalate to
`mag.run_failed`: execution escaped the typed business model. `kill` retires an
actor and voids late outputs; it is not a routeable semantic error.

## Cycles and absence

Cycles are ordinary graph topology. Their termination must be visible in typed
exits or in a finite runtime actor such as the retry gate; an unrestricted
feedback edge may never terminate.

There is no implicit "fire when X did not happen." Absence becomes positive
data produced by a timeout, failure, or another actor whose result can be
composed normally.
