-- starter/lib/graph.lua — orchestrator template + spawn_graph schema/advertise.
--
-- Pure helpers extracted from agentic_workflow.lua during the Phase 1
-- refactor. No module-level mutable state.

local provider_contract = require("lib.contracts.provider")
local tool_contract     = require("lib.contracts.tool")

local M = {}

-- Virtual source name we register `spawn_graph` under. There is no
-- plugin by this name on the bus — agentic_workflow.for_tool_gate
-- intercepts the gate-forwarded `spawn-graph-tool.tool.invoke` and
-- drops it before targeting tries to deliver to a non-existent peer.
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

function M.build_orchestrator_graph(opts)
  opts = opts or {}
  local provider = opts.provider or "ollama"
  local model = opts.model
  local system = opts.system or ""
  local user_text = opts.user_text or ""

  local wrap_args = {
    provider = provider,
    prompt   = user_text,
  }
  if type(system) == "string" and #system > 0 then
    wrap_args.system = system
  end
  if type(model) == "string" and #model > 0 then
    wrap_args.model = model
  end

  return {
    nodes = {
      {
        id       = "wrap",
        reasoner = "provider-wrapper",
        args     = wrap_args,
        fanout   = {
          ["in"] = provider_contract.PROVIDER_OUT,
          out    = {
            tool_contract.TOOL_CALLS,
            provider_contract.FINAL_ANSWER,
          },
        },
      },
      { id = "tools",    reasoner = "tool-executor", args = {} },
      { id = "adapt",    reasoner = "adapter",       args = {} },
      { id = "terminal", reasoner = "terminal",      args = {} },
    },
    edges = {
      { from = "wrap",  to = "tools",    type = tool_contract.TOOL_CALLS },
      { from = "wrap",  to = "terminal", type = provider_contract.FINAL_ANSWER },
      { from = "tools", to = "adapt" },
      { from = "adapt", to = "wrap" },
    },
  }
end

return M
