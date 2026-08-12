# The Nefor Manifesto

Nefor is a hyperextensible runtime for composing tools, plugins, and interfaces in Lua.

**Small core. User-owned config. Replaceable everything else.**

Think Neovim as a runtime: a programmable core, process plugins, and a Lua config you own. The engine spawns processes, routes lines, hosts Lua, and stamps identity. Your `init.lua` decides what exists, how it talks, what gets persisted, and which interface sits on top.

Nefor does not require LLMs. Models, scripts, tools, agents, orchestrators, and plain interfaces are composable units when wired in. The bundled starter is one distribution, not the product boundary.

## The world this manifesto protects

This manifesto guides development inside the Nefor repository: the engine, shipped libraries and plugins, starter, and public boundaries maintained here. Projects that use Nefor are free to build different architectures and do not have to follow it.

Every feature added to Nefor must fit the world described below. If it does not, discard it or redesign it. In the rare case where the feature exposes a fundamental mistake in that world, reconsider the world explicitly before changing the code. Existing code and an attractive feature are evidence, not automatic exceptions.

The implementation will change. Rust, Lua, processes, JSON Lines, NCP, MAG, and the terminal interface are current choices unless a published contract deliberately makes one of them stable. What should survive those choices is the user's real ability to find, change, remove, and replace behavior without patching unrelated machinery.

## Protect the engine

The engine is Nefor's most important and most protected part. Feature work does not normally change it. New behavior belongs in the components the engine runs, the libraries they use, or the composition that connects them.

Change the engine only after designing the engine change explicitly and establishing that Nefor lacks a general mechanism which ordinary components cannot provide with equal authority. The mechanism must remain domain-independent. It must not assume chats, models, tools, agents, graphs, providers, approvals, or any other product vocabulary.

A feature does not belong in the engine merely because every current distribution uses it, because it would be faster there, or because putting it there removes a wrapper. A feature fails this principle when a non-agent product must invent agent concepts to use the engine, or when removing a starter feature requires an engine change.

## Put product decisions in composition

The user's composition owns which components exist, how they connect, and which product policies apply. This includes routing, approvals, persistence, fallback, provider choice, workflow, interface, and translation between product vocabularies.

A decision is user-owned only when its effective source and selected default are visible, and the user can change it without editing the engine or unrelated component code. Hiding policy behind an opaque helper, implicit import order, search path, namespace trick, or fixed initialization sequence does not make it composable.

Defaults are welcome. They must be explicit choices made by a distribution, not privileges that the engine or a supposedly generic component quietly grants itself.

## Provide replaceable mechanisms, not a mandatory framework

Nefor's shipped Lua libraries make common behavior easier to assemble. They
provide focused mechanisms, useful implementations, and explicit composition
seams. They do not form a mandatory application framework, and being shipped
gives an implementation no special authority.

A user's composition may use a library implementation unchanged, configure or
wrap it, replace part of it, provide an independent implementation of the same
interface, or ignore the shipped libraries and work directly against the
engine's process and routing boundaries. Consumers depend on declared behavior
and interfaces, not on a particular library function's identity or private
state.

Reusable libraries must not hide a deployment-shaped actor, workflow, or
product behind a generic-looking API. Complete assemblies belong to
configurations and examples. When configurations repeatedly copy the same
mechanism, extract that mechanism into a focused library with replaceable
components; do not make one configuration inherit another, and do not make
bypassing the library harder than implementing the underlying boundary
directly.

## Make components replaceable in practice

A capability declares the inputs, outputs, configuration, environment, and lifecycle behavior it needs. It depends on those boundaries, not on a particular neighbor's name, private data, timing accident, or undocumented optional behavior.

Plugins should pass the bash-tool test: each does a coherent job through declared inputs and outputs and does not know its neighbors. Heavy, stateful, concurrent, or platform-specific work can still be a good plugin. Cross-component wiring and product policy belong in Lua.

Some relationships genuinely need translation, aggregation, authorization, correlation, or routing. Give that work one visible, cohesive home. Do not hide it inside an endpoint, duplicate it across peers, or split it into pieces that must reach back into each other's private state. A new boundary is justified by a real difference in contract, authority, lifecycle, deployment, failure containment, or replacement—not by a new filename.

## Make authority and lifecycle knowable

For state or work that crosses a boundary, it must be possible to answer, at the relevant scope:

- who decides its canonical meaning or outcome;
- who currently holds it and under what rule it can move;
- who may change it;
- who may create, settle, cancel, release, or destroy it; and
- who reconciles uncertainty after failure.

The answer may be a component, a partitioned rule, or a distributed protocol. It cannot be an unqualified claim that something “owns” the state. Two components must not independently claim the same authority unless an explicit arbitration rule resolves the conflict.

Resources must not outlive the run or component that created them unless responsibility is explicitly transferred. Process exit alone is not a lifecycle design for resources that cross a process boundary.

## Do not hide important transitions or loss

Accepted work, external effects, authority transfers, cancellation, resource release, data loss, and terminal outcomes must have an authoritative source and a machine-observable path to whoever must act on them.

A component must not silently guess malformed data into a valid shape, repair an unknown state, discard accepted work, or report success while relevant child work is unsettled. Loss is allowed only where the composition or contract declares a loss policy and reports enough information for the responsible component to account for it. Human-readable logs alone are not such accounting.

Unknown failures remain unknown failures. Do not force them into a familiar category merely to keep a closed list looking complete.

## Represent meaning directly

If a distinction changes routing, validation, authority, lifecycle, or interpretation, represent and validate it explicitly. Do not infer it from an optional field, a string prefix or suffix, payload shape, magic value, or a contradictory set of flags.

Strings and open objects are normal at process and serialization boundaries. Once data enters code that acts on its meaning, parse it into the clearest closed representation the language can express. Unknown values may be preserved for forwarding, but must not be treated as understood.

Authoritative identity must come from the boundary that establishes it. A sender's unverified claim must not silently become attribution, routing authority, or permission to speak for a namespace.

## Design for a trusted local system

Nefor runs locally for one user. The engine, configuration, and installed components are part of the same trusted system. They are not hostile tenants defending themselves from one another.

Do not add authentication, authorization, sandboxing, message-signing, anti-spoofing, or permission machinery to defend Nefor components from hypothetical malicious peers. Web and multi-tenant security practices do not apply merely because Nefor passes messages or runs processes. Such machinery adds boundaries, state, and failure modes while solving a threat Nefor does not claim to face.

A real boundary may still require protection when Nefor communicates with something outside this trusted local system, handles credentials, or executes work under a different operating-system authority. State the concrete threat and protect that boundary only. Do not spread its restrictions through unrelated local components.

Contract validation, explicit failure, authoritative identity, lifecycle ownership, and prevention of invalid state remain required. They preserve the system's intended behavior; they are not security features and do not need an attacker to justify them.

## Publish contracts deliberately

Treat a boundary as a contract when separately released implementations use it, replacement is promised across it, persisted or external data depends on it, multiple implementations exist, or compatibility is claimed publicly.

A contract states what crosses the boundary, what the behavior means, how lifecycle and failure work, and what stability is promised. It also makes clear who validates it and who may assign names within it. A private boundary may remain an implementation detail, but Nefor must not advertise compatibility or replacement where no contract exists.

Shipping a type, Lua API, wire shape, or file format does not automatically freeze it. Engine-specific definitions and current protocol details belong in their own docs. This manifesto protects the architectural relationship, not today's vocabulary.

## Keep distributions outside the core

The starter demonstrates one useful Nefor product. Its chat surface, providers, tools, sessions, approvals, MAG workflow, and defaults may be opinionated. Their prominence gives them no authority over the engine or other distributions.

A distribution may define a strong local vocabulary and local contracts. Other distributions must be able to remove it, replace it, or build something unrelated without patching the engine or fabricating the starter's concepts.

Replaceability is semantic, not a synonym for “separate process.” Processes are today's useful isolation boundary, but a contract should require a language, runtime, transport, or deployment form only when that property is itself essential to the promise being made.

## Add less, remove cleanly

Before adding a core primitive, shared contract, registry, adapter layer, extension point, or reusable component, name the current use that fails without it. Hypothetical reuse, convenience, and fewer wrappers are not enough. A bounded executable experiment is enough to test an idea, but it does not create a compatibility promise.

When one design replaces another, choose explicitly:

- **supersession:** remove the old path;
- **migration:** preserve a stated contract while users or data move; or
- **bounded coexistence:** keep both with explicit authority, arbitration, and an end condition.

Do not leave two unnamed eras alive behind flags or compatibility shims. Deprecated paths gain no new dependents. New work should make the active source of behavior easier, not harder, to locate.

## The feature test

A proposed Nefor feature should have clear answers to these questions:

1. What concrete use fails without it?
2. Why is this the right layer? If it enters the engine, why can no ordinary component provide it with equal authority, and how is it domain-independent?
3. Which decisions remain under the user's composition, and where are their defaults visible?
4. What does each component require, and can a conforming replacement satisfy those requirements without private knowledge?
5. Where do cross-component relationships live?
6. For state, work, resources, failure, and cancellation, who has authority and lifecycle responsibility at each point?
7. Which distinctions affect behavior, and where are they validated?
8. Is any boundary now a promised contract? If so, what exactly is stable?
9. What can be removed, and does replacement leave one clear active design?

If these answers are unclear, the feature is not ready. If the answers contradict this manifesto, redesign or reject it rather than widening Nefor around the feature.
