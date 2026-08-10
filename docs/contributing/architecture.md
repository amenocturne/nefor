# Contributor architecture

The short authoritative model is in [`../architecture.md`](../architecture.md). This guide maps that model onto current code ownership so contributors can decide where a change belongs.

## Runtime path

The `nefor` engine starts the Lua composition, spawns process plugins, and routes newline-delimited strings through the Lua `dispatch` hook. Lua implements NCP routing and actor composition. The starter wires providers, the conversation manager, tool gates, sessions, MAG, and chat surfaces. A lead turn submits a typed MAG program; the MAG compiler emits a generic artifact and the MAG plugin executes it as a run-scoped actor constellation.

The engine is deliberately not the orchestrator. It owns process and log substrate. Durable sessions, conversation projections, approval policy, agent turns, and UI state are composed above it.

## Ownership map

| Area                                                      | Canonical owner                                    | Boundary                                                                         |
| --------------------------------------------------------- | -------------------------------------------------- | -------------------------------------------------------------------------------- |
| Process lifecycle, CLI/path resolution, in-memory bus log | `engine/`                                          | Must not interpret workflow-specific bodies.                                     |
| Wire envelopes and plugin SDK                             | `crates/nefor-protocol`, `crates/nefor-plugin-sdk` | Shared process contract; avoid composition policy.                               |
| SSE parsing                                               | `crates/nefor-sse`                                 | Transport mechanism shared by providers.                                         |
| MAG parser, evaluator, types, diagnostics, artifact API   | `crates/nefor-mag`                                 | Domain-neutral; concrete Nefor graph knowledge belongs in libraries.             |
| Provider/tool/TUI/MAG capabilities                        | `plugins/*`                                        | Separate processes; self-contained capability implementations.                   |
| NCP, actor, event, replay primitives                      | `lua/core`                                         | Small engine-grade Lua mechanism.                                                |
| Reusable runtime behavior                                 | `lua/libs/*`                                       | Bus-aware mechanisms with explicit extension seams, not starter policy.          |
| Concrete product composition and prompts                  | `starter/`                                         | Shipped opinions, defaults, actor wiring, MAG library seed, and chat extensions. |
| Headless surface                                          | `cli-config/` and `lua/libs/cli`                   | CLI composition over the same runtime mechanisms.                                |
| Distribution and verification                             | `tools/`, `justfile`, `.github/workflows`          | Discover binaries from Cargo metadata; do not maintain parallel plugin lists.    |

Important shared Lua owners include `conversation-manager` for canonical conversation facts and projections, `agentic-loop` for per-turn MAG submission, `lead-workflow` for run controls, `mag-workspace` and `mag-run-bindings` for MAG integration, `sessions` for persistence/resume, `chat` for config-agnostic UI state/view mechanics, and `tool-validator` for the policy extension boundary. Starter modules supply the concrete settings and extensions.

## Placement tests

A plugin should pass the bash-tool test: it has clear stdin/stdout capability semantics and does not need to know neighboring plugin names or global policy. Cross-plugin translation and policy belong in Lua.

Within Lua, ask whether the behavior is reusable mechanism or shipped opinion. Reusable state machines and adapters live in `lua/libs`; selected providers, prompt text, policy registration, statusline/slash composition, and actor wiring live in `starter`.

Within MAG, keep the compiler generic. Runtime concepts enter through namespaced libraries, foreign declarations, semantic type witnesses, and the artifact boundary. The compiler must not grow special cases for agents, providers, tools, graphs, or sinks.

## State and persistence

Session jsonl and provenance use the resolved shared session root. The engine retains an in-memory log while running, but Lua/session mechanisms own durable behavior. The conversation manager owns canonical conversation identity and facts; UI and agentic-loop consumers project from those facts rather than each reconstructing provider history. MAG runs are run-scoped, and nested-run ownership is preserved for status and control.

Across pre-public minor versions, compatibility is not guaranteed. Prefer one canonical model and remove the replaced path rather than carrying dual representations. Patch releases within a minor line remain backward compatible.
