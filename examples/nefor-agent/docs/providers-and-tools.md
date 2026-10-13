# Providers, tools, and permission policy

Providers and tools cross several independent boundaries: process transport, canonical event translation, model advertisement, capability allowlists, and human permission policy. Adding a binary is therefore not always enough; preserve every boundary the new component participates in.

## Providers

The starter supports three provider descriptor kinds:

- `mock` for deterministic local scenarios;
- `openai` for an OpenAI-compatible endpoint;
- `chatgpt` for the bundled ChatGPT provider.

OpenRouter is an optional `openai` instance, not another descriptor kind. Enable it with `NEFOR_ENABLE_OPENROUTER=1` and provide its key through the generic provider's `OPENAI_PROVIDER_API_KEY` input.

Providers may expose exact namespaced usage values through conversation-manager. The starter wires `chatgpt/subscription` and `openrouter/session-total` into `/usage` and independent statusline segments; either list can be configured without changing the other. OpenRouter begins unknown, records only authoritative non-BYOK USD contributions in the session JSONL, and rebuilds the total on replay without another provider request. On a session switch the surface retains the account-level ChatGPT snapshot but clears the session-local OpenRouter total until its owner reconstructs it.

These values are deliberately separate from completion token usage. Provider completion `usage` and `conversation.provider.context_usage` drive the context-window bar and turn statistics; they do not imply subscription quota, price, or an exposed `usage_id`.

Configure those through [`config.active.providers`](customization.md#starter-settings). A new provider kind requires composition code. Prefer `libs.compositors.provider.spawn_spec` when the provider can satisfy the canonical translator interface; see [Configuration and composition](customization.md#providers).

A provider subprocess speaks NCP, but provider request/history/stream types are a higher-level contract. `generic-provider` owns canonical type declarations and concrete providers translate to them. Do not treat those event kinds as NCP itself.

The ChatGPT provider owns the `web_search` descriptor and advertises it when
the composition-selected tool gate announces liveness. Configuration selects
the ordinary catalog name in an agent's toolset; it does not reproduce the
descriptor or pass a process-wide switch. Selected + advertised + matching
provider lowers to ChatGPT's native Responses tool with
`external_web_access: false`. Unselected, stale, and wrong-provider descriptors
are omitted. Native execution bypasses local invocation and approval, but its
sanitized lifecycle is recorded through the same canonical tool exchange and
standard UI as routed tools.

## Config-defined read-only tools

`libs.read-only-tools` exposes a supported registration seam. Extra tools are
built with [`libs.tool`](../../../lua/libs/tool/init.lua), the constructor every
Nefor tool uses:

```lua
local tool = require("libs.tool")

return require("libs.read-only-tools").build {
  include = {
    "discover_instruction_files",
    "skill",
  },
  extra_tools = {
    tool.define {
      name = "my_read_tool",
      description = "Look up a value without changing external state.",
      parameters = {
        type = "object",
        properties = {
          query = { type = "string" },
        },
        required = { "query" },
      },
      display = {
        compact = { label = "My read tool" },
        expanded = { label = "My read tool", fields = {} },
        result = { kind = "content", fields = {} },
      },
      -- What a call means for a caller with each approval policy, and what
      -- the tool adds to its description for such an agent. Every policy
      -- needs an entry.
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      prepare = tool.nothing,
      execute = function(args, call)
        call.ok("result for " .. args.query)
        -- On expected failure, call call.err("message") instead.
      end,
    },
  },
}
```

Base tools are opt-in: `build {}` advertises none. The available base tools are repository instruction discovery and ordinary workflow skill loading; shell/process capabilities handle directory listings, searches, and Python analysis. `tool.define` checks the schema and Nefor's `display` metadata when the tool is defined, and rejects schema keywords its validator does not support. Every schema is closed: arguments not declared in `properties` are rejected, and invalid arguments never reach the tool; the caller receives an error such as ``my_read_tool: unknown argument `q`; accepted arguments: query``. `execute` settles exactly once through `call.ok` or `call.err`. `approval` says what a call means for a caller with each approval policy; nothing is assumed, so a definition without `approval` for every policy, or without `prepare`, fails when the tool is defined. All tools built here share source identity `read-only-tools`.

Spawn the actor before `tool-gate`; it advertises on the gate's hello. A tool intended for delegated agents may also need entries in the MAG toolset/allowlist, gate policy, and startup readiness.

## Permission policy

Registration makes a tool available; it does not authorize every call. What an
agent may do is the approval policy of its toolset (`ReadOnly` or `Default`),
and each tool says what a call means under each policy. The gate decides from
the two and the permission mode; see the [permissions guide](permissions.md).

Nothing is implied. A tool defines, in its definition, an entry for every
approval policy and its `prepare` step, or the definition fails:

```lua
local tool = require("libs.tool")

tool.define {
  name = "publish_note",
  -- description, parameters, display ...
  approval = {
    ReadOnly = {
      -- "safe", { "unsure", reason }, { "denied", reason }, or a function
      decide = { "denied", "it publishes a note" },
      -- Appended to the description for an agent with this policy.
      describe = "Your approval policy is ReadOnly, so every call to this tool is rejected.",
    },
    Default = {
      decide = function(args, call)
        if args.draft then return "safe" end
        return "unsure", "it publishes to everyone"
      end,
      describe = tool.nothing,
    },
  },
  prepare = tool.nothing,
  execute = function(args, call) call.ok("published") end,
}
```

`tool.nothing` is the explicit "this tool has none". A decision function that
returns no valid decision, or raises, ends the call with an error; failing to
decide is never a decision. The gate has no per-tool settings that stand in
for a definition; its one setting is `--deny <tool>`.

Where a configuration chooses what one of Nefor's tools means for a `Default`
agent, it says so where it builds the tool:

```lua
tools.git_worktree_actor_spec {
  decide_create = { "unsure", "it creates a Git worktree and a branch" },
  decide_open = { "unsure", "it opens a Git worktree outside the session" },
}
tools.basic_actor_spec {
  max_read_bytes = N,
  classify_command = validator.classify_command,
  classify_argv = validator.classify_argv,
  decide_write = function()            -- write_file for a Default agent
    if lead_workflow.has_approved_plan() then return "safe" end
    return "denied", "write_file requires an approved plan"
  end,
}
```

Commands are classified by `libs.tool-validator`, which the command tools and
`mag-apply` decide from. All three options are required:

```lua
local pm = require("nefor-pm")
local da = require("libs.tool-validator.da")
pm.install { da.package { commit = "<trusted-da-commit>" } }

local validator = require("libs.tool-validator").build {
  classifier = pm.bin("da", "da"),
  -- The classifier's flags for every approval policy. A flag such as da's
  -- `--ml` model is listed for the policies that should use it.
  classifier_args = {
    ReadOnly = { "--read-only", "--git", "read" },
    Default = { "--read-only", "--git", "read,add,commit", "--ml" },
  },
  -- Approve narrow commands before the classifier is asked; {} for none.
  command_fastpaths = {
    function(command, policy)
      return policy == "Default" and command:match("^my%-cli%s+read%s") ~= nil
    end,
  },
}
actor.spawn(validator)
-- validator.classify_command(policy, command) -> "safe" | "unsure" | "denied", reason
-- validator.classify_argv(policy, argv)       -> the same, for an argument vector
```

The classifier reads the command on stdin and exits 0 to approve, 1 to defer
or 2 to deny; pass it no mode flag. A classifier that cannot be run, or that
exits with any other status, raises: the tool call ends with an error that
says classification failed and why, never with a policy answer.
`classify_argv` writes the vector as the equivalent POSIX command, every
element single-quoted, and classifies that text, so a program and a script are
judged by the same rules. Fast paths are trusted policy predicates.

## Tool plugins

For tools that need a separate process, author an NCP plugin that executes capabilities and define the model-facing tools in Lua with `libs.tool`. `libs.compositors.tools.capability_tools_spec` wraps such a process as a tool source: it advertises the Lua definitions to `tool-gate`, validates each forwarded invocation, and sends `<name>.capability.invoke` to the plugin. Use [Plugin authoring](../../../docs/plugin-authoring.md) for transport and lifecycle, and see the bundled `basic-tools` and `git-worktree` plugins with `lua/libs/basic-tools` and `lua/libs/git-worktree` for the current shapes.

Tool advertisement, invocation, and approval are ecosystem protocols layered over NCP. Replacing `tool-gate` means owning their routing, provenance, permission, replay, and instruction-file integration.
