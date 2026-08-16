# Architecture

nefor runs as a small engine plus user-owned Lua composition. The shipped starter layers MAG, providers, tools, approvals, sessions, and interfaces on top of that substrate; those choices are replaceable composition, not engine behavior.

| Layer                         | What it owns                                                                                                                                                                                                                                               | What it avoids                                                                                                                                                                                                         |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Engine / bus                  | Spawning the exact plugin commands registered by Lua, bridging stdio, hosting Lua, routing raw lines through the Lua dispatch hook, stamping in-memory log entries with origin and timestamp, and reporting typed process-termination facts.               | Parsing NCP bodies for routing, owning sessions or selecting their root, writing session jsonl, discovering plugin directories/inventory, deciding whether a plugin exit shuts down the runtime, or owning TLS policy. |
| Plugins                       | Self-contained capabilities over stdin/stdout: providers, tools, TUI, MAG runtime, registries, test actors. Provider plugins own HTTPS policy and construct clients through `nefor-provider-http`, which adds native system roots to bundled WebPKI roots. | Cross-plugin policy or hard-coded knowledge of how another plugin is used.                                                                                                                                             |
| Lua composition and libraries | Dispatch, NCP handshake/routing semantics, explicit plugin commands and distribution resolution, actor spawning, sessions and their root, lifecycle/shutdown policy, approvals, UI reducers, CLI/TUI surfaces.                                             | Heavy provider/tool implementation that belongs in a process plugin.                                                                                                                                                   |
| MAG                           | Pure namespaced evaluation; libraries define typed graph data, foreign capabilities, validation, and lowering into a generic `Artifact`.                                                                                                                   | Knowing actors, factories, shell, sinks, or Nefor wire types; those live in libraries and runtime contracts.                                                                                                           |

## The decoupling rule

A plugin capability and the logic that uses it are separate concerns. Adding a new workflow or policy should normally touch Lua composition, Lua libraries, or MAG programs, not provider/tool plugin internals.

Bash-tool test: a plugin should feel like a self-contained utility you could run from a shell, then compose elsewhere. If code names neighboring plugins, rewrites their event shapes, or decides global policy, it is glue; put it in Lua.

## What belongs in Lua

Lua owns behavior that is composition-specific or bus-aware:

- NCP handshake and default routing (`lua/core/ncp.lua`).
- Actor spawning and dispatch wiring (`examples/nefor-agent/init.lua`, `lua/core/actor.lua`).
- Session root selection, persistence, replacement, resume, and replay (`examples/nefor-agent/init.lua`, `lua/libs/sessions`). Rust remains session-blind; the starter defaults the root to `$NEFOR_DATA_DIR/sessions` and permits `NEFOR_SESSIONS_DIR` to replace it.
- Approval and tool validation policy (`lua/libs/tool-validator`, `lua/libs/lead-workflow`).
- Provider/tool adapters and interface reducers.
- Chat event/key sequencing through `libs.chat.controller`, assembled from named handler groups with `libs.chat.dispatch`. The starter supplies its command handler as one visible group; consumers can use the defaults, wrap or replace a handler with an explicit duplicate policy, or bypass the controller and use `nefor-tui` primitives directly.
- MAG submission/control and workspace management.
- Plugin process lifecycle policy. Rust reports typed termination facts with authoritative plugin identity; composition decides whether a fact warrants `nefor.engine.shutdown { code, reason, grace_ms }`. The first shutdown request owns the complete request and its single cooperative grace window.

Pure reusable mechanisms live under `lua/core` or `lua/libs`; example opinions and concrete wiring live under `examples/nefor-agent`.

### Conversation authority and turn orchestration

The example deliberately separates durable meaning from transient execution:

- `lua/libs/conversation-manager` is the canonical authority for recorded conversation facts. It validates and sequences those facts, derives provider-neutral projections for the TUI/CLI, and supplies the conversation context used to create model calls. Session replay feeds recorded facts back through this owner to reconstruct state; consumers do not infer a second transcript from provider or workflow traffic.
- `lua/libs/agentic-loop` orchestrates the current lead turn. It queues input, starts the configured MAG turn program, manages ephemeral provider chats, and coordinates interruption and compaction requests. Its queue and active-turn bookkeeping are process state, not a competing conversation record.

This split keeps replay authoritative without making the conversation manager responsible for live workflow scheduling. Surface reducers render conversation-manager projections and separately observe transient workflow state.

#### Transcript disposition

Not every recorded message is conversation. A message fact carries an explicit
`visibility`: `transcript` (the default — ordinary conversation) or
`diagnostic` (model context that no surface renders as conversation). The
conversation manager validates it, keeps both dispositions in the context
projection it hands to providers, and lets a message narrow to `diagnostic`
exactly once at its terminal fact — content that already streamed can be
retracted, but nothing recorded as diagnostic is ever promoted back.

The typed provider boundary (`structured-output`) is the current use. An
attempt whose text fails schema validation, and the bounded correction prompt
that answers it, are recorded diagnostic: the next round still sees them, and
the surface never shows a rejected attempt's prose, reasoning, or streamed
deltas as an assistant message, nor its correction as a user message. The
surface retracts what a narrowed message already streamed, so the accepted
answer occupies the position the turn's provider round started in, and a turn
that exhausts its correction budget settles as one failure with no
metadata-only answer beside it. Attempt counts and violations remain visible on
the existing `mag.diagnostic` channel, which carries no candidate output.

The provider boundary treats finalized tool arguments as untrusted model output. A call is executable only when its `function.arguments` decodes to a JSON object; empty, malformed, scalar, null, and array values are quarantined. The malformed assistant call is not recorded. Instead, the canonical conversation records a bounded user correction naming the call and diagnostic, then requests another completion. This keeps every reconstructed OpenAI assistant tool call provider-valid across continuation and session replay.

### Model selection

Selecting a model is a request to the provider that owns it, not a fact the
surface may assume. `lua/libs/chat/model_selection` is the single primitive
behind every entry point — the `/model` picker, `/model <provider> <model>`, and
`/model <model>`:

- The surface records the requested `(provider, model)` pair plus the state a
  failure restores, then emits `chat.model.set`.
- `chat.model.set_ack` is correlated against that pending pair. The matching ack
  adopts provider, model, that pair's reasoning default and that pair's context
  window in one patch, before any turn runs. An ack for another pair — a
  provider hello, a replayed session, a superseded request — changes nothing.
  With no pending selection, an ack is adopted only for the already-active
  provider.
- `chat.model.set_failed`, or the pending provider leaving the connected state,
  restores the captured pair and clears the request. The provider compositor is
  the one place that knows a selection was in flight, so it correlates the
  provider's untargeted turn error into `chat.model.set_failed` rather than an
  anonymous system message.
- `/model <model>` resolves against the catalogs reported by
  `chat.models.listed`. It selects only when exactly one catalog offers that
  model; two or more, or none, produce runnable qualified commands instead of a
  guess. `/model <provider> <model>` is the user's own authority and does not
  require catalog membership, so a model id containing slashes works.

Route enforcement is deliberately out of scope: the composition targets a
selection at a provider actor, and that actor decides what it will serve. Nefor
does not template per-model endpoints or carry routing metadata that would let
one provider claim another's models. There is no generic seam that could verify
a qualified pair before the owning provider answers, so an unserved pair
surfaces as that provider's rejection (rollback) rather than as a pre-flight
error. Adding such a seam would be a separate design with its own authority and
lifecycle questions.

## Provider HTTPS trust

Network-owning Rust providers construct HTTPS clients through the
`nefor-provider-http` crate. It preserves reqwest/rustls's bundled WebPKI public
roots and adds certificates loaded from the platform trust store (including
macOS Keychain roots). Individual native entries which the platform loader
rejects are counted and logged without certificate contents; if loading yields
no certificates and reports errors, provider startup fails visibly. Hostname
and certificate-chain validation remain enabled. This is provider mechanism,
not engine or Lua policy, and there is no custom-PEM configuration surface.

### Asynchronous engine callbacks

Detached runtime tasks never invoke Lua directly. Process stdout, stderr, and exit observations are serialized per process onto a broker-owned callback channel. Channel readiness is an explicit broker wake source: one broker turn invokes a bounded callback batch under the same single-task Lua ownership as inbound dispatch, then drains every bus event those callbacks appended before returning to the idle select loop. A queued callback is itself retained readiness, so arrivals immediately before select cannot lose their wake; dropping the broker receiver during shutdown prevents later tasks from entering a torn-down VM.

## Control plane

The lead operates on run statuses and results, not by inspecting every internal message in a graph. MAG run results are delivered inline on bus events, and the lead-workflow tools expose graph status and output lookup as control-plane conveniences.

Persistence is not an engine promise. The engine keeps an in-memory log for dispatch/replay while the process is alive. Long-term session and MAG-output persistence are Lua/plugin mechanisms owned by the starter libraries and MAG kernel integration.
