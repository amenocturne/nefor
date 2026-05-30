-- starter/agentic-loop/topology.lua
--
-- Orchestrator graph template — the four-node wrap/tools/adapt/terminal
-- topology agentic-loop submits to reasoner-graph for every new chat
-- turn. Extracted from the old `lua/core-libs/graph/init.lua` during
-- the core/libs split.
--
-- The type-constant inputs (provider_out, final_answer, tool_calls)
-- are passed via `opts` rather than fetched from generic-provider /
-- generic-tool so this module stays decoupled from those libs. The
-- agentic-loop call site threads them in from
-- `require("libs.generic-provider")` / `require("libs.generic-tool")`.

local M = {}

-- Build the orchestrator template graph.
--
-- opts fields:
--   provider      — orchestrator provider name (informational; the
--                   picker is the source of truth at runtime)
--   model         — orchestrator model name (informational)
--   reasoning_effort — optional reasoning effort for new provider chats
--   system        — system prompt; threaded into the wrap node's args
--   user_text     — current user message; threaded into the wrap
--                   node's args as `prompt`
--   tool_allowlist — optional list of tool names; threaded into wrap
--                   args so the orchestrator's chat catalog is
--                   filtered to those names
--   provider_out  — type tag the wrap node consumes (e.g. ProviderOut)
--   final_answer  — fanout-out type tag for the terminal branch
--   tool_calls    — fanout-out type tag for the tool-executor branch
--
-- Returns the graph spec `{ nodes = {...}, edges = {...} }` ready to
-- send through `tool.invoke { name = "spawn_graph", args = { graph } }`.
function M.build_orchestrator_graph(opts)
  opts = opts or {}
  local system = opts.system or ""
  local user_text = opts.user_text or ""

  assert(type(opts.provider_out) == "string",
    "topology.build_orchestrator_graph: opts.provider_out (string) required")
  assert(type(opts.final_answer) == "string",
    "topology.build_orchestrator_graph: opts.final_answer (string) required")
  assert(type(opts.tool_calls) == "string",
    "topology.build_orchestrator_graph: opts.tool_calls (string) required")

  -- Provider + model are NOT baked into wrap_args. The picker (state.config
  -- on agentic-loop) is the single source of truth: every reasoner firing
  -- — orchestrator wrap + every responder/etc. node spawned via
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
  if type(opts.reasoning_effort) == "string" and #opts.reasoning_effort > 0 then
    wrap_args.reasoning_effort = opts.reasoning_effort
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
          ["in"] = opts.provider_out,
          out    = {
            opts.tool_calls,
            opts.final_answer,
          },
        },
      },
      { id = "tools",    reasoner = "tool-executor", args = {} },
      { id = "adapt",    reasoner = "adapter",       args = {} },
      { id = "terminal", reasoner = "terminal",      args = {} },
    },
    edges = {
      { from = "wrap",  to = "tools",    type = opts.tool_calls },
      { from = "wrap",  to = "terminal", type = opts.final_answer },
      { from = "tools", to = "adapt" },
      { from = "adapt", to = "wrap" },
    },
  }
end

return M
