# Architecture — four execution layers

nefor executes work across four layers. Each layer has a fixed job; the most
common architectural bug is putting code one layer too low.

| Layer             | What it is                             | What it does                                                                                                                                                                                                         | What it never does                                                                                       |
| ----------------- | -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Engine            | Dumb string bus                        | Route strings between plugin processes, stamp envelopes, persist the session log, invoke the Lua dispatch hook                                                                                                       | Know about actors, runs, workflows, signals                                                              |
| Plugins           | Heavyweight OS processes, boot-spawned | Capability providers. Own all provider quirks (e.g. `chatgpt-provider` owns everything about talking to OpenAI). Each exposes its own API — including whether cancellation exists at all                             | Contain workflow logic. Know how their capability is being used                                          |
| Lua (trait layer) | Actor factories + kernel               | Define the primitive actor shapes (`llm`, `run-tool`, `human`, `sink`, …) with declared I/O contracts and explicit signal handlers. Host the kernel: spawn / kill / send over in-memory actors, routing, correlation | Define composed workflows. Hold two levels of abstraction (building blocks _and_ things built from them) |
| MAG               | The language                           | Instances and composition. `(agent {:id "..." ...} : IN -> FinalAnswer)` creates a live actor constellation. `agent` is a MAG stdlib function composing primitives (the bare agentic loop)                           | Reach below the primitive contracts                                                                      |

## The decoupling rule

A plugin's capability and the logic that uses it are separate concerns. All
workflow logic is MAG programs; plugins are pure capability.

**The test: adding a new workflow pattern touches zero plugins.** If it means
editing a handler table or a plugin, the layering is broken.

## What earns a Lua-level primitive

Something is Lua-level **iff MAG cannot express it**: either it crosses a plugin
boundary (`llm`, `run-tool`, `human`) or it is irreducible runtime machinery
(`llm` transcript state, `sink` routing). Everything composable from primitives
lives in the MAG stdlib — `agent` included. The lead still writes
`(agent {:id "..." ...} : ...)`; it is a stdlib function, not a runtime feature.

## Control plane

The lead operates on run statuses and results, never on the data flowing
between actors. Within a run, actors pass typed messages along routes; the
run's result reaches the lead inline on `mag.run_result` (and `mag-eval`
returns the terminal node's output inline). Per-node file persistence
(`runs/<run_id>/<node>.output`) is host-gated on the shared Lua tree: the mag
plugin points its kernel VM's `package.path` at the `--lua-root` the composition
resolves (`starter/init.lua`), so `require("output-persistence")` — and thus
persistence — is **active** whenever that tree resolves. That is the deployed
default: `starter` passes `--lua-root`, and installed configs fall back to the
data-root pm checkout (`<data>/nefor/lua`); a live session persists a full run's
node outputs under `sessions/<sid>/mag/runs/<run_id>/`. It degrades to a warned
no-op (`persisted = false`) only when no Lua tree resolves — a kernel loaded
without `--lua-root` and outside any repo/data tree, as in bare plugin unit
tests (`LuaHost::load_kernel(path, None)`), which is the vantage point that reads
as "inactive". Either way the inline result on `mag.run_result` stays the live
channel; persisted paths are a control-plane convenience the lead may read. The
graph is the data bus; the lead is the control plane.
