# Architecture — four execution layers

nefor executes work across four layers. Each layer has a fixed job; the most
common architectural bug is putting code one layer too low.

| Layer | What it is | What it does | What it never does |
|---|---|---|---|
| Engine | Dumb string bus | Route strings between plugin processes, stamp envelopes, persist the session log, invoke the Lua dispatch hook | Know about actors, runs, workflows, signals |
| Plugins | Heavyweight OS processes, boot-spawned | Capability providers. Own all provider quirks (e.g. `chatgpt-provider` owns everything about talking to OpenAI). Each exposes its own API — including whether cancellation exists at all | Contain workflow logic. Know how their capability is being used |
| Lua (trait layer) | Actor factories + kernel | Define the primitive actor shapes (`llm`, `run-tool`, `human`, `loop-counter`, `sink`, …) with declared I/O contracts and explicit signal handlers. Host the kernel: spawn / kill / send over in-memory actors, routing, correlation | Define composed workflows. Hold two levels of abstraction (building blocks *and* things built from them) |
| MAG | The language | Instances and composition. `(agent "prompt")` creates a live actor constellation. Stdlib templates (`agent`, `gate`, `retry-bounded`) are MAG functions composing primitives | Reach below the primitive contracts |

## The decoupling rule

A plugin's capability and the logic that uses it are separate concerns. All
workflow logic is MAG programs; plugins are pure capability.

**The test: adding a new workflow pattern touches zero plugins.** If it means
editing a handler table or a plugin, the layering is broken.

## What earns a Lua-level primitive

Something is Lua-level **iff MAG cannot express it**: either it crosses a plugin
boundary (`llm`, `run-tool`, `human`) or it is irreducible runtime machinery
(`loop-counter` state, `sink` routing). Everything composable from primitives
lives in the MAG stdlib — `agent` included. The lead still writes
`(agent "prompt")`; it is a stdlib function, not a runtime feature.

## Control plane

The lead operates on statuses and output file paths, never on data flowing
between actors. Every actor's output persists to a per-node file; downstream
graphs reference prior outputs by path. The graph is the data bus; the lead is
the control plane.
