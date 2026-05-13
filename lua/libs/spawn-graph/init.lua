-- lua/libs/spawn-graph/init.lua
--
-- Spawn-graph tool protocol contract — the constants and schema used to
-- advertise `spawn_graph` to a tool-gate. The reasoner-graph binary
-- handles the canonical `tool.invoke { name = "spawn_graph" }` shape;
-- this module exposes the gate-side advertisement primitives so any
-- consumer (tool-gate wrapper, agentic-loop, etc.) can emit the right
-- envelope without baking the protocol shape into its own code.
--
-- Lives under `lua/libs/` (shared multi-plugin contract) rather than
-- inside either plugin's lib, because reasoner-graph defines the shape
-- and tool-gate consumes it for advertisement — exactly the shared
-- contract role of `libs.generic-provider` and `libs.generic-tool`.

local M = {}

-- Virtual source name we register `spawn_graph` under. There is no
-- plugin by this name on the bus — the tool-gate wrapper intercepts
-- the gate-forwarded `<SPAWN_GRAPH_SOURCE>.tool.invoke` and drops it
-- before targeting tries to deliver to a non-existent peer.
M.SPAWN_GRAPH_SOURCE = "spawn-graph-tool"

function M.spawn_graph_schema()
  return {
    type = "object",
    description = "Submit a reasoner-graph run and return its results.",
    properties = {
      graph = {
        type = "object",
        description = "The graph topology: { nodes: [...], edges: [...] }.",
      },
      on_node_failure = {
        type = "string",
        enum = { "abort", "continue" },
        description = "Failure policy. Defaults to abort.",
      },
    },
    required = { "graph" },
  }
end

function M.advertise_body(gate_name)
  return {
    kind   = gate_name .. ".tools.advertise",
    source = M.SPAWN_GRAPH_SOURCE,
    tools  = {
      {
        name        = "spawn_graph",
        description = "Submit a reasoner-graph run and return its terminal results.",
        parameters  = M.spawn_graph_schema(),
      },
    },
  }
end

return M
