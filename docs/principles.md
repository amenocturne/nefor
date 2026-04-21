# nefor principles

This document captures the design and writing principles that nefor has committed to. It exists so contributors — including future us — can check their work against a stable reference, instead of having to reconstruct the discipline from scratch every time.

When in doubt while writing code, a spec, a doc, or a commit message: read the relevant section below, then decide.

Four sections:

1. [Architecture principles](#architecture-principles) — how the system is shaped
2. [Engine / protocol principles](#engine--protocol-principles) — what the engine commits to
3. [Writing principles](#writing-principles) — how we express things in prose
4. [Documentation structure principles](#documentation-structure-principles) — how artifacts are organised

---

## Architecture principles

### Three-layer opinion model

nefor has three layers, each with increasing opinion:

1. **Combinators** (least opinion) — pure algebra over `Reasoner<C>`. The only escape is to build your own substrate.
2. **Engine** (medium opinion) — plugin host, NCP broker. If you disagree, rewrite the engine while speaking NCP and inherit the plugin ecosystem for free.
3. **Plugins** (most opinion) — frontends, harnesses, widgets. Total user choice.

Every layer has an escape hatch one level up. Users never commit to the whole stack without options.

### Contracts, not implementations

The **APIs at layer boundaries are more valuable than the implementations behind them.** Our combinator crate is the reference algebra; our engine binary is the reference NCP implementation; our plugins are reference compositions. None of them are canonical — the contracts are. A third party implementing the same contract inherits the ecosystem.

Consequence: specs, laws, and protocol documents are first-class artifacts. Treat them with the discipline of product surface.

### Unix philosophy, all the way down

Small tools, composed with clear interfaces, over monoliths with configuration surface. No single binary tries to be everything. The engine brokers; plugins specialise. Cross-cutting concerns (logging, metrics, custom buses) are plugins, not features.

When tempted to add "just one more thing" to the engine: write a plugin instead.

### YAGNI on speculative design

Build only what the current problem requires. We have one reference implementation of each layer and the problem is tractable; don't design for hypothetical second users until they exist. When they arrive, the contracts are already honest and the refactor follows.

### No v2 that carries v1's legacy

We do not ship backwards-incompatible major versions that try to live alongside the old paradigm. If a rewrite is different enough to need "v2," it's a different project — a new binary, a new name, its own repo. The model is grep / ripgrep, ls / exa, make / just: similar functionality, distinct tools, each internally coherent. The anti-model is React's concurrent mode / hooks / class components coexisting indefinitely — a codebase whose cognitive surface is the sum of every era.

Consequences for how we work:

- **No tech-debt deferral.** We do not accept "fix it properly in v2." There is no v2. Do it right now, or acknowledge the limitation in the current contract.
- **No compatibility shims.** Breaking changes are fine during development; once shipped, the contract is the contract. If the contract is wrong, it's a new project.
- **No paradigm cohabitation.** When a better idea lands, we do not keep the old paradigm around "for users who haven't migrated." Migration is a fork-and-replace, not a flag.

Corollary: take the time to get contracts right on the first pass. Short-term pain, long-term stability. Software like `grep(1)` or `ls(1)` has worked for decades without changing shape — that's the target.

### No stringly-typed state

Every piece of state that carries meaning — errors, kinds, reasons, codes, modes, phases — is a closed enum. Never a string, never an integer sentinel, never a free-form tag that downstream code pattern-matches on substrings. Strings exist at exactly two boundaries: serialization to the wire, and human-readable output for logs or UI. Everywhere else, variants.

Corollaries:

- **No string-sniffing to classify errors.** If two error conditions need different handling, they are different enum arms. Adding a `Custom(String)` escape hatch is a smell; stop and add a real variant instead.
- **Display/Error mapping is explicit per arm.** `thiserror`'s per-variant `#[error("...")]` attributes, not a single catch-all format string that tries to describe every case.
- **Pin message strings in tests** if the wire carries them (e.g., NCP `error.message`). An accidental reword must break a test, not silently change protocol-level behaviour.
- **Parse errors carry structured context.** Not `Error(String)` with a freeform diagnostic; `Error { kind, field, reason }` with enum-typed slots that consumers can match on.

This is why `nefor-protocol`'s `ParseError` has variants like `InvalidReadyBody(InvalidReadyReason)` and `InvalidSystemBody { kind, reason }` — the broker branches on variants, not message text. When you find yourself writing `msg.contains("...")` to decide what to do, stop: that's a missing enum arm.

### Runner / broker split

The engine binary has exactly two subsystems:

- **Runner** — spawns declared subprocesses with direct `Command::new(binary).args(...)`, bridges stdio, detects exit. Does not parse NCP; does not know what the bus is.
- **Broker** — parses NCP envelopes, stamps `from` (from runner-assigned name) and `ts`, validates system messages, broadcasts event messages, enforces bounded per-peer queues.

Runner does not invoke shell, does not manage env vars, does not handle cwd beyond the `<plugin-dir>/<name>/` convention. Plugins that need shell features, env, supervision, or daemon patterns wrap themselves in a user-chosen wrapper script and expose that as their `command`. A future community plugin can provide these as reusable services.

This split is why nefor is cross-platform for free: the engine never assumes `/bin/sh` exists. It only assumes the declared binary is exec'able — which is how `std::process::Command` works on every supported platform.

### Do we even need it?

The meta-principle behind every other principle in this section. Every time a feature, field, abstraction, or dependency is proposed — before asking "how do we build it?", ask "do we actually need it?" Default to removing requirements, not accommodating them.

> Junior engineers write beautiful code with premature abstractions and optimizations. Middles write code that gets the job done. Seniors ask if we even need it.

Deleting a requirement is strictly better than implementing one: no surface area, no tests, no edge cases, no drift, no cognitive tax on future readers. Most "defensible" features are defensible in isolation and lose when weighed against the cost of carrying them.

Track record in nefor — each of these was initially defensible, and each collapsed under the question:

- Capabilities in NCP → dropped (can't enforce them; install-trust is honest)
- WASM sandbox → dropped (same reason)
- `plugin_joined` / `plugin_left` / `detach` / bootstrap roster → dropped (plugins advertise themselves; liveness is a plugin convention)
- `args` / `env` / `cwd` / `build` fields on spawn → dropped (userspace composes via wrapper scripts or Lua)
- `/bin/sh` dependency in the engine → dropped (direct exec; shell is a plugin's choice)
- Plugin name in attach body → dropped (engine assigns from spawn config)
- `InvalidSystemBody(String)` stringly-typed escape hatch → dropped (structured enum)
- Plugin name override at runtime → dropped (name is a compile-time property of the plugin)

How to apply it:

1. When a feature is proposed, first ask "what breaks if we don't add this?"
2. If the answer is "some users would need a wrapper" — decline the feature; users write wrappers.
3. If the answer is "the system is unusable" — proceed, but pick the smallest thing that fixes it.
4. Before merging any addition, check whether something adjacent can be deleted in the same commit. If yes, do it.

YAGNI, no-v2, no-stringly-typed-state, contracts-over-implementations — they're all specific applications of this reflex. Default to "no" until the need is real and irreducible.

---

## Engine / protocol principles

These principles shape NCP and the engine's runtime behaviour. They also appear in the [NCP spec §1](../protocol/v0.1/spec.md#1-overview) as design principles — this section is the longer-form version for reference and future contributors.

### Minimal

The engine understands only the system messages defined in the spec. Everything else — event bodies, sub-protocols, request/response patterns, addressing conventions — is opaque to the engine.

When adding functionality: first ask "can a plugin do this?" If yes, it's a plugin. Only things that require engine-level privilege (managing the connection lifecycle, stamping delivery facts, brokering the bus) belong in the engine.

### Broadcast

The bus is a fan-out mechanism. Every event message reaches every connected plugin. Plugins filter by matching on body fields. No subscriptions, no routing tables, no addressing enforcement.

This is load-bearing because it gives observability, replay, debug-tapping, and metrics plugins for free. Adding routing would cost more than it saves.

### Non-spoofable delivery

`from` and `ts` are engine-stamped. A plugin cannot lie about sender identity or arrival time. These are the engine's authoritative word — plugins consuming messages trust them unconditionally.

Never add an envelope field that plugins control and the engine forwards unchecked. If plugins want to claim something, put it in body where it's clearly plugin speech.

### Reject, don't repair

The engine validates every message and either delivers it unchanged or drops it with a named `error` back to the sender. It never silently corrects, reinterprets, or best-effort-forwards malformed input.

Every fault has a code and a named recipient. Plugins are never left guessing whether their message was accepted, modified, or dropped. No silent behaviour, anywhere.

### Engine narrates delivery; plugins narrate content

The engine's vocabulary is the envelope plus the system kinds. All other semantics — kinds, request/response, addressing, scheduling, role definitions — are plugin speech. The engine doesn't adjudicate plugin-level semantics.

When writing spec or docs, ask: "is this about delivery or about content?" Delivery goes in the spec; content goes in plugin-authoring.

### Plugins are processes

The engine does not regulate what plugins do inside their own process. Language, runtime, concurrency model, subprocesses, filesystem access, network calls, other systems — none of it is NCP's concern. The engine's entire contract with a plugin is NCP.

This is how the spec stays small: by refusing to care about anything that isn't the bus.

### Sub-protocols emerge

Plugins define their own message shapes under their plugin-name namespace. NCP does not register, arbitrate, or validate sub-protocols. De-facto standards emerge from quality and adoption — the Telescope / lazy.nvim pattern, not LSP's central arbitration.

When a pattern feels universal enough to document, it goes in `docs/plugin-authoring.md`, not in the spec.

---

## Writing principles

These apply to spec text, docs, READMEs, inline comments, commit messages — any prose we write about nefor.

### Voice: hard lines + exit doors

When a rule is binding, state it flatly. When you've drawn a line, immediately point at the sanctioned alternative.

> ✅ "No other envelope fields are permitted. Plugins needing more metadata put it in body."
>
> ✅ "Plugins needing to move larger payloads should arrange that outside of NCP."
>
> ❌ "Implementations MAY support larger payloads through out-of-band mechanisms not specified by this document."

The first form asserts authority and tells the reader what to do next. The second is ceremony.

### Describe engine behaviour, not plugin behaviour

The spec describes what the engine does. It does not prescribe what plugins do internally. Any sentence of the form "the plugin makes/decides/uses/avoids X" where X is plugin-internal is a smell — rewrite as engine-speak.

> ✅ "The engine rejects the ready with `protocol_version_mismatch` if the declared version does not match."
>
> ❌ "The plugin must check that its protocol version matches the engine's."

The first is binding and describes observable engine behaviour. The second is a wish about plugin implementation we cannot enforce and have no reason to care about.

### No RFC hedging where a bright line is honest

Use SHOULD and MAY for genuinely optional behaviour. Do not dilute a MUST into a SHOULD because prose culture expects hedging. If the engine enforces it, say MUST. If the engine doesn't enforce it at all, the statement probably doesn't belong in the spec — move it to docs.

### Trust the principle, don't hedge with examples

A parenthetical list of examples is usually standing in for "any N you might imagine" — and when the rule is correctly stated, the examples add nothing. Delete them.

> ✅ "the language, the runtime, the concurrency model"
>
> ❌ "the language, the runtime, the concurrency model (tokio, ZIO, async Python, bare threads, single-threaded event loop, whatever)"

The abstraction proves itself; the example list weakens it by suggesting you needed to illustrate.

### "Free-form" is a smell

When documenting a field, ask: could we pick a standard format here? If yes, pick it and enforce it. "Free-form" is tech debt that consumers pay — every downstream parser copes with the long tail of what people send.

Candidates that should use standards: versions (SemVer), timestamps (ISO-8601), IDs (UUID where unique-per-universe matters), URIs. Candidates that are genuinely free-form: human-readable diagnostic strings (where machine-readable form already exists in `code`).

### Self-contained field descriptions

Every rule that applies to a field should be reachable from that field's own paragraph, either inline or via cross-reference. A reader drilling into one field should not be able to violate a rule that was declared earlier in the document.

Three things to include at every field:

1. **Format constraints** (type, length, character set, reserved values)
2. **Dispatch consequences** — if malformed, what error surfaces
3. **Cross-references** to the broader rule when the inline statement is a summary

### State the "why" for every parameter

Every required field should answer "why does this exist?" in its own description. A reader should not have to infer the purpose from context. If a field's purpose is only diagnostic, say so ("sent as-is for plugin-side consumption; engine makes no decisions"). If it gates behaviour, state that ("engine rejects on mismatch").

### Generic names in examples

JSON examples in the spec use `plugin-a`, `plugin-b`, `plugin-name`. Not actual plugin names from the ecosystem. Specific names elevate some plugins above others and tie spec examples to implementation choices that may change. `docs/plugin-authoring.md` and other ecosystem documents may use real names.

---

## Documentation structure principles

### Spec and docs are different contracts

- **Spec** — frozen per version, describes enforced behaviour, stability-critical. Reader is implementing conformance.
- **Docs** — lives, describes conventions and style, grows with the ecosystem. Reader is writing an everyday plugin.

Never mix them. Numbered sections in the spec should not contain SHOULD-language advisory content; that's a signal to split.

### Version the contract, not the implementation

Major boundaries get their own versioned artifact (spec, combinator laws, capability WITs). Reference implementations track their own semver. A third party implementing the contract can upgrade the implementation without breaking the contract, and vice versa.

### Each doc file has one audience

- `protocol/v0.1/spec.md` — implementers of NCP.
- `docs/plugin-authoring.md` — people writing plugins.
- `docs/glossary.md` — anyone wanting a fast term lookup.
- `docs/principles.md` — contributors editing anything in the repo.
- `CLAUDE.md` / `AGENTS.md` — AI agents working in this repo.
- `README.md` — humans landing on the repo.

When a file's audience drifts, split it.

### No orphan documents

Every doc under `docs/` should be reachable from the README, from a sibling doc, or from a spec cross-reference. If a document has no inbound link, it's either wrong, obsolete, or needs to be connected.

---

## How this document evolves

New principles get added here when:

1. A contributor (or reviewer) catches a violation that couldn't be explained by an existing principle.
2. A design decision gets made that affects multiple future edits.
3. A conversation discovers a rule that was implicit and should be explicit.

Existing principles get refined or retired only when:

1. A real consumer forces a change.
2. A contradiction with a more fundamental principle is discovered.

Principles in this document are not personal preferences. They are decisions we've made in light of specific tradeoffs, and they should survive until the tradeoffs change.
