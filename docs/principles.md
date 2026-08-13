# nefor principles

The [Nefor Manifesto](manifesto.md) governs feature and architectural decisions. This document records supporting design and writing principles within that world; where the two conflict, follow the manifesto.

This document captures the design and writing principles that nefor has committed to. It exists so contributors — including future us — can check their work against a stable reference, instead of having to reconstruct the discipline from scratch every time.

When in doubt while writing code, a spec, a doc, or a commit message: read the manifesto and the relevant section below, then decide.

Four sections:

1. [Architecture principles](#architecture-principles) — how the system is shaped
2. [Engine / protocol principles](#engine--protocol-principles) — what the engine commits to
3. [Writing principles](#writing-principles) — how we express things in prose
4. [Documentation structure principles](#documentation-structure-principles) — how artifacts are organised

---

## Architecture principles

### Current layer model

nefor has four practical layers:

1. **Engine / bus** — least opinion. Spawns processes, bridges stdio, hosts Lua, and routes raw lines through the Lua dispatch hook. It owns no workflows and no sessions.
2. **Plugins** — isolated capability processes. Providers, tools, interfaces, registries, and MAG run outside the engine and communicate through lines.
3. **Lua composition and libraries** — bus-aware policy and wiring. NCP semantics, sessions, approval policy, provider/tool adaptation, actor spawning, UI reducers, and example defaults live here.
4. **MAG programs/kernel** — graph execution used by the starter lead workflow. MAG is a shipped capability, not the only place workflow logic exists.

Every file gets one layer assignment. Mismatch is the most common architectural bug.

### Contracts, not implementations

The APIs at layer boundaries are more valuable than the implementations behind them. The current public boundary is process + JSON Lines + Lua-owned NCP conventions, described in docs/protocol.md. A plugin or config that speaks that boundary should not depend on private implementation details of the engine or another plugin.

### Unix philosophy, all the way down

Small tools, composed with clear interfaces, over monoliths with configuration surface. The engine brokers; plugins specialise; cross-cutting concerns such as persistence, policy, metrics, and routing conventions are Lua/plugin work.

When tempted to add just one more thing to the engine: write a plugin or Lua library instead.

### YAGNI on speculative design

Build only what the current problem requires. Do not design public docs around hypothetical future transports, protocol suites, or compatibility layers until they ship.

### No v2 that carries v1's legacy

We do not ship backwards-incompatible major versions that try to live alongside the old paradigm. If a rewrite is different enough to need v2, it is a different project.

Consequences for how we work:

- **No tech-debt deferral.** Do it right now, or acknowledge the limitation in the current contract.
- **No compatibility shims by default.** During pre-public development, prefer deleting replaced shapes over carrying fallbacks.
- **No paradigm cohabitation.** When a better idea lands, do not keep the old paradigm around as a flag.

### No stringly-typed state

Every piece of state that carries meaning — errors, kinds, reasons, codes, modes, phases — should be a closed enum or table of known values at the layer that owns it. Strings exist at serialization boundaries and for human-readable output.

### Runner / broker split

The engine binary has two relevant subsystems:

- **Runner** — starts the exact command registered by Lua with direct `Command::new(binary).args(...)`, bridges stdio, and detects exit. It does not discover binaries or installation layouts, invoke a shell, set per-plugin cwd, or parse NCP.
- **Broker** — receives raw plugin lines, appends Lua-published emissions to an in-memory log, drains that log through Lua dispatch, and writes direct deliveries to plugin stdin. It does not own NCP envelope parsing, session jsonl, ready timeouts, queue-overflow protocol messages, or shutdown system messages.

Lua's NCP framework owns ready/ready_ok, event classification, default routing, replay-on-attach, and direct error replies.

### Do we even need it?

Before asking how do we build it, ask do we actually need it? Default to removing requirements, not accommodating them.

1. When a feature is proposed, first ask what breaks if we do not add this.
2. If the answer is some users would need a wrapper, decline the feature; users write wrappers.
3. If the answer is the system is unusable, proceed, but pick the smallest thing that fixes it.
4. Before merging any addition, check whether something adjacent can be deleted in the same commit.

---

## Engine / protocol principles

These principles shape the current Lua-owned NCP/string-bus behavior. The behavior document is docs/protocol.md.

### Minimal engine

The Rust engine understands process spawning, raw line movement, Lua dispatch, direct delivery, and in-memory log bookkeeping. It does not parse plugin event bodies or enforce plugin sub-protocols.

When adding functionality: first ask can Lua or a plugin do this? If yes, keep it out of the engine.

### Lua-owned protocol semantics

Ready handshake, event classification, default routing, replay-on-attach, direct error replies, and wrapper callbacks live in lua/core/ncp.lua. Docs should describe that ownership plainly instead of attributing strict NCP behavior to Rust.

### Published events, not magical global broadcast

The bus contains what Lua or wrappers explicitly publish with nefor.engine.send. Default routing broadcasts to other ready peers unless a target, legacy peer prefix, or wrapper override narrows delivery. A line a plugin emits but its wrapper does not republish is not bus traffic.

### Engine narrates delivery; plugins narrate content

The engine can say where a raw line came from and when a bus entry was recorded. Plugin-level semantics — kinds, request/response, addressing, scheduling, roles — are plugin or Lua speech.

### Plugins are processes

The engine does not regulate what plugins do inside their own process. Language, runtime, concurrency model, subprocesses, filesystem access, network calls, and external systems are outside the engine contract.

### Sub-protocols emerge

Plugins define their own message shapes under their plugin-name namespace. nefor does not centrally register or validate these. When a pattern feels universal enough to document, it goes in docs/plugin-authoring.md, not in engine code.

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

- `docs/protocol.md` — current shipped protocol behavior.
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
