-- Model-facing tools over the git-worktree capability plugin.
--
-- The `git-worktree` process executes `git_worktree_create` and
-- `git_worktree_open`; these definitions own the tools a model or MAG node
-- calls, and forward validated arguments to the capability of the same name.
--
--   require("libs.git-worktree").tools {
--     invoke = function(call, capability, args) end,
--     decide_create = "safe" | { result, reason } | function(args, call) end,
--     decide_open   = the same,
--   } -> { tool definitions }
--
-- `decide_create` and `decide_open` are the composition's explicit choice of
-- what creating and opening a worktree mean for a `Default` caller. For a
-- `ReadOnly` caller creating is always denied and opening is safe.

local tool = require("libs.tool")
local approval = require("libs.tool.approval")
local tool_display = require("libs.chat.tool_display")

local M = {}

local function display(label, receipt)
  return {
    compact = {
      label = label,
      primary = { label = "path", select = { source = "args", path = "path" }, kind = "path" },
    },
    expanded = { label = label, fields = tool_display.fields({}) },
    result = { kind = "receipt", text = receipt, fields = tool_display.fields({}) },
  }
end

local function path_properties(with_base)
  local properties = {
    repository = { type = "string" },
    path = { type = "string" },
    branch = { type = "string" },
  }
  if with_base then properties.base = { type = "string" } end
  return properties
end

function M.tools(opts)
  if type(opts) ~= "table" or type(opts.invoke) ~= "function" then
    error("git-worktree.tools: invoke must be a function", 2)
  end
  if opts.decide_create == nil or opts.decide_open == nil then
    error("git-worktree.tools: decide_create and decide_open are not defined; they are what "
      .. "creating and opening a worktree mean for a Default caller", 2)
  end
  local invoke = opts.invoke
  local CREATES = "it creates a Git worktree and a branch"
  local function forward(capability)
    return function(args, call) invoke(call, capability, args) end
  end
  return {
    tool.define {
      name = "git_worktree_create",
      description = "Create a fresh Git worktree; existing paths or branches are errors.",
      parameters = {
        type = "object",
        properties = path_properties(true),
        required = { "repository", "path", "branch", "base" },
        additionalProperties = false,
      },
      display = display("create worktree", "worktree created"),
      context = {},
      approval = {
        ReadOnly = { decide = { "denied", CREATES }, describe = approval.read_only_rejection(CREATES) },
        Default = { decide = opts.decide_create, describe = tool.nothing },
      },
      prepare = tool.nothing,
      execute = forward("git_worktree_create"),
    },
    tool.define {
      name = "git_worktree_open",
      description = "Open and validate an explicitly named existing Git worktree.",
      parameters = {
        type = "object",
        properties = path_properties(false),
        required = { "repository", "path", "branch" },
        additionalProperties = false,
      },
      display = display("open worktree", "worktree opened"),
      context = {},
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = opts.decide_open, describe = tool.nothing },
      },
      prepare = tool.nothing,
      execute = forward("git_worktree_open"),
    },
  }
end

return M
