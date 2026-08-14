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

## Provider HTTPS trust

Network-owning Rust providers construct HTTPS clients through the
`nefor-provider-http` crate. It preserves reqwest/rustls's bundled WebPKI public
roots and adds certificates loaded from the platform trust store (including
macOS Keychain roots). Individual native entries which the platform loader
rejects are counted and logged without certificate contents; if loading yields
no certificates and reports errors, provider startup fails visibly. Hostname
and certificate-chain validation remain enabled. This is provider mechanism,
not engine or Lua policy, and there is no custom-PEM configuration surface.

## Control plane

The lead operates on run statuses and results, not by inspecting every internal message in a graph. MAG run results are delivered inline on bus events, and the lead-workflow tools expose graph status and output lookup as control-plane conveniences.

Persistence is not an engine promise. The engine keeps an in-memory log for dispatch/replay while the process is alive. Long-term session and MAG-output persistence are Lua/plugin mechanisms owned by the starter libraries and MAG kernel integration.
