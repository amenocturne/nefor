# Actor model

## Interface

An actor is anything that can receive messages. That is the whole interface;
it never grows. Messages include signals — signals are ordinary messages with
reserved kinds, nothing special at the bus level.

Actors are lightweight in-memory constructs with workflow-scoped lifecycles.
They are not engine constructs and not processes: the engine knows plugins and
how to execute Lua, nothing more.

From the runtime's point of view an actor is used as a function: fire its
input message, black box until it returns its output. Completion is
returning — per activation; a node activated twice returns twice. What the
runtime does with the returned output (routing, rules) is described in
ir.md.

The kernel holds one actor inventory: a single map from actor id to instance,
shared across all factories. Spawn inserts, kill deletes, routing consults
nothing else.

**Actors are unaware of the bus.** The mag plugin, like any plugin, receives
every bus message; what it does with them is its own business. It filters and
delivers to actors through the kernel — so the kernel is an actor's entire
world: every message an actor receives arrives from the kernel, every message
it emits goes through the kernel. No actor subscribes to, or even knows about,
the bus underneath.

**Actors hold no lifecycle authority.** Composing, spawning, and killing are
environment operations — actors never spawn actors; composition that depends
on runtime results is expressed as rules evaluated by the environment (see
ir.md). An actor receives messages and emits messages, nothing else.

## Factories

A factory is a Lua-defined constructor: given an actor id and setup params, it
creates the actor and confirms creation with a ready message for that id.
Factories are the trait layer — abstract shapes that become concrete actors
when MAG instantiates them.

Two obligations:

- **Accept the id, sign with the id.** Every outbound message of an instance
  carries its actor id. With several actors running, each is individually
  addressable and none is affected by messages meant for another.
- **Declare the contract.** Each factory states the input shapes it accepts,
  the output shapes it produces, and the signals it handles. Composition
  type-checks against declared contracts; nothing selects inputs by sniffing
  their shape. In a cyclic composition (the agentic loop), which output exits
  the cycle is a type fact — a declared algebraic type, built from sums and
  products, e.g. `ProviderOut -> (ToolCalls | FinalAnswer)` — never a
  position heuristic. The input side carries firing semantics the same way:
  single type fires per message, union fires on any, product fires on all
  (see ir.md, Firing).

## Lifecycle

Spawning is asynchronous: between spawn request and ready, the rest of the
constellation may already be sending to the new id. The convention:

1. On spawn request the kernel registers the id in the inventory and opens a
   pending mailbox for it.
2. Messages routed to a registered-but-not-ready id queue in that mailbox —
   discriminated by actor id, not replayed from the bus. Full-bus replay would
   make every actor pay for history it doesn't need; the mailbox delivers
   exactly the messages addressed to you, in arrival order.
3. The factory creates the instance and emits ready; the kernel drains the
   mailbox to it and routes live from then on.

The mailbox lives in the kernel because it is the flip side of kill —
spawn = reserve id + queue, kill = unroute + drop — and factory-side buffering
would have every factory reimplement the same machinery.

## Signals

The conventions are Unix-shaped. You can write any actor you want and ignore
all of it — but the system expects the shape, and non-conforming actors lose
the graceful path, not the system its correctness.

| Signal | Analog | Semantics |
|---|---|---|
| kill | SIGKILL | The kernel unilaterally deletes the id from the inventory and drops its pending mailbox — no further messages route to it, nothing waits for it, no handler can veto or delay the removal. The dying instance is then handed one final kill message: handling is optional, but an actor holding live external work (an open provider request) implements the handler to abort it |
| drain | SIGTERM | Finish or abort current work, flush outputs, then die. The handleable convention actors are expected to implement |

The universal set stays aggressively small: each entry taxes every actor
forever. The bar is "the system cannot work without it", not "handy".

**No injected behavior.** There is no wrapper that composes standard handlers
around actor logic. An actor's source is the whole truth: reading a factory
definition shows exactly which signals it handles and how. Conformance is
verified by reading, not trusted to machinery.

## Cancellation

There is no generic cancellation contract. Each plugin's API is the statement
of what cancellation means for it:

- Pure plugins (request → response, no state, no expensive resources — e.g.
  `da`): nothing to cancel. Fire-and-forget; an unread response is already
  clean. Zero overhead by construction.
- Stateful plugins (e.g. a provider with an open streaming call): the abort
  primitive is part of that plugin's own API, with its own shapes.

Factories that use a cancellable capability write an explicit signal handler
with the plugin-level message shapes inline — low-level details live where
low-level knowledge already is (the factory necessarily knows the plugin's
request shapes; it knows the abort shape too). When several factories share a
stateful plugin, the plugin's Lua-side adapter module owns the messy envelope
once and factories call it: reuse via library, not via protocol.
