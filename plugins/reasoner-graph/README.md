# reasoner-graph

NCP v0.1 plugin: dumb scheduler for graphs of reasoners. Renamed from
`dag-scheduler` — cycles are allowed (it's a graph, not a DAG).

This plugin owns graph topology, scheduling, and the wire shape for
dispatching nodes (`<reasoner>.run_node`) and reaping their results
(`graph.node_result`). The plugins it invokes (reasoners, tool plugins,
sub-agents) do the actual work.

See the parent spec
`projects/software/active/nefor/specs/nefor-agent-and-reasoner-types-spec.md`
§3 for the full wire contract — kinds, fields, ack lifecycle, per-firing
state carry, combinator-driven fanout.

## Run

Spawned by the engine over stdio. See `starter/init.lua` for the spawn
block. Logs to stderr (`tracing`); stdout is the NCP channel.
