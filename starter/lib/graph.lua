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
  local system = opts.system or ""
  local user_text = opts.user_text or ""

  -- Provider + model are NOT baked into wrap_args. The picker (state.config
  -- on agentic-loop) is the single source of truth: every reasoner firing
  -- — orchestrator wrap + every responder/dummy/etc. node spawned via
  -- spawn_graph — falls through `provider_run_node`'s `args.provider or
  -- cfg.provider` precedence to the live config. Per-node routing is still
  -- opt-in: a caller that wants a specific provider/model on a specific
  -- node sets args.provider / args.model on that node explicitly.
  local wrap_args = {
    prompt = user_text,
  }
  if type(system) == "string" and #system > 0 then
    wrap_args.system = system
  end
  -- Optional tool-name allowlist for the orchestrator's chat. When set,
  -- provider-wrapper forwards it as `chat.create.tools = <list of names>`
  -- so the provider only advertises those names to the model in
  -- `chat.complete` (matching the agent reasoner's existing pattern for
  -- sub-agent firings). Without this the lead's chat sees the full
  -- catalog including reasoner-graph internals like `spawn_graph` —
  -- which the lead can call directly, bypassing the role-keyed
  -- `dispatch-graph` contract and bottoming out in
  -- `reasoner '<role>' not connected` errors.
  if type(opts.tool_allowlist) == "table" then
    wrap_args.tool_allowlist = opts.tool_allowlist
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
