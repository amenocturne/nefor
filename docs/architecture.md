# Architecture

nefor runs as a small engine plus user-owned Lua composition. The shipped starter layers MAG, providers, tools, approvals, sessions, and interfaces on top of that substrate; those choices are replaceable composition, not engine behavior.

| Layer                         | What it owns                                                                                                                                                       | What it avoids                                                                                               |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| Engine / bus                  | Spawning plugin processes, bridging stdio, hosting Lua, routing raw lines through the Lua dispatch hook, stamping in-memory log entries with origin and timestamp. | Parsing NCP bodies for routing, owning sessions, writing session jsonl, knowing workflows or approvals.      |
| Plugins                       | Self-contained capabilities over stdin/stdout: providers, tools, TUI, MAG runtime, registries, test actors.                                                        | Cross-plugin policy or hard-coded knowledge of how another plugin is used.                                   |
| Lua composition and libraries | Dispatch, NCP handshake/routing semantics, actor spawning, provider/tool wiring, sessions, approval policy, UI reducers, CLI/TUI surfaces.                         | Heavy provider/tool implementation that belongs in a process plugin.                                         |
| MAG                           | Pure namespaced evaluation; libraries define typed graph data, foreign capabilities, validation, and lowering into a generic `Artifact`.                           | Knowing actors, factories, shell, sinks, or Nefor wire types; those live in libraries and runtime contracts. |

## The decoupling rule

A plugin capability and the logic that uses it are separate concerns. Adding a new workflow or policy should normally touch Lua composition, Lua libraries, or MAG programs, not provider/tool plugin internals.

Bash-tool test: a plugin should feel like a self-contained utility you could run from a shell, then compose elsewhere. If code names neighboring plugins, rewrites their event shapes, or decides global policy, it is glue; put it in Lua.

## What belongs in Lua

Lua owns behavior that is composition-specific or bus-aware:

- NCP handshake and default routing (`lua/core/ncp.lua`).
- Actor spawning and dispatch wiring (`starter/init.lua`, `lua/core/actor.lua`).
- Session persistence and resume (`lua/libs/sessions`).
- Approval and tool validation policy (`lua/libs/tool-validator`, `lua/libs/lead-workflow`).
- Provider/tool adapters and interface reducers.
- MAG submission/control and workspace management.

Pure reusable mechanisms live under `lua/core` or `lua/libs`; starter opinions and concrete wiring live under `starter`.

## Control plane

The lead operates on run statuses and results, not by inspecting every internal message in a graph. MAG run results are delivered inline on bus events, and the lead-workflow tools expose graph status and output lookup as control-plane conveniences.

Persistence is not an engine promise. The engine keeps an in-memory log for dispatch/replay while the process is alive. Long-term session and MAG-output persistence are Lua/plugin mechanisms owned by the starter libraries and MAG kernel integration.
