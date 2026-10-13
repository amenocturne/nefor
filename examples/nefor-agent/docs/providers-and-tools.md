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
the ordinary catalog name in an agent's `tools` list; it does not reproduce the
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
      approval = { decide = require("libs.tool.approval").reads() },
      execute = function(args, call)
        call.ok("result for " .. args.query)
        -- On expected failure, call call.err("message") instead.
      end,
    },
  },
}
```

Base tools are opt-in: `build {}` advertises none. The available base tools are repository instruction discovery and ordinary workflow skill loading; shell/process capabilities handle directory listings, searches, and Python analysis. `tool.define` checks the schema and Nefor's `display` metadata when the tool is defined, and rejects schema keywords its validator does not support. Every schema is closed: arguments not declared in `properties` are rejected, and invalid arguments never reach the tool; the caller receives an error such as ``my_read_tool: unknown argument `q`; accepted arguments: query``. `execute` settles exactly once through `call.ok` or `call.err`. `approval.decide` says what a call means under the calling agent's approval policy: `approval.reads()` declares a tool that only reads, so a `ReadOnly` agent may call it; a tool that declares nothing is rejected for a `ReadOnly` agent and follows the gate's configured setting otherwise. All tools built here share source identity `read-only-tools`.

Spawn the actor before `tool-gate`; it advertises on the gate's hello. A tool intended for delegated agents may also need entries in the MAG toolset/allowlist, gate policy, and startup readiness.

## Permission policy

Registration makes a tool available; it does not authorize every call. What an
agent may do is its `tool_approval_policy` (`ReadOnly` or `Default`), and each
tool says what a call means under a policy. The gate decides from the two and
the permission mode; see the [permissions guide](permissions.md).

A tool declares its meaning in its definition:

```lua
local approval = require("libs.tool.approval")

tool.define {
  name = "publish_note",
  -- description, parameters, display ...
  approval = {
    -- (policy, args, call) -> "safe" | "unsure" | "denied", reason
    decide = function(policy, args)
      if policy == approval.ReadOnly then return approval.denied, "it publishes a note" end
      if args.draft then return approval.safe end
      return approval.unsure, "it publishes to everyone"
    end,
    -- Appended to the description for an agent with that policy.
    describe = { ReadOnly = "Your approval policy is ReadOnly, so every call to this tool is rejected." },
  },
  execute = function(args, call) call.ok("published") end,
}
```

`approval.reads()` and `approval.writes(reason)` cover the two common cases. A
tool that declares nothing follows the gate's configured setting
(`tool_gate.auto_tools` / `prompt_tools` / `default_action`) for a `Default`
agent and is rejected for a `ReadOnly` agent.

Commands are classified by `libs.tool-validator`, which the command tools and
`mag-apply` decide from:

```lua
local pm = require("nefor-pm")
local da = require("libs.tool-validator.da")
pm.install { da.package { commit = "<trusted-da-commit>" } }

local validator = require("libs.tool-validator").build {
  classifier = pm.bin("da", "da"),
  -- Optional. Replace a policy's classifier flags outright:
  classifier_args = { ReadOnly = { "--read-only", "--git", "read" } },
  -- Optional. The classifier's opt-in model flag, added to Default only:
  model_flag = nil,
  -- Optional. Approve narrow commands before the classifier is asked:
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
or 2 to deny. A missing or unusable classifier makes the command denied with
an actionable reason; it never becomes approval or an unresolved wait.
`classify_argv` writes the vector as the equivalent POSIX command, every
element single-quoted, and classifies that text, so a program and a script are
judged by the same rules. Fast paths are trusted policy predicates.

The starter wires the classification into `basic_actor_spec` and gives
`write_file` its plan rule there (`decide_write`): under `Default` it needs a
current approved write-review plan, and it is always rejected for a `ReadOnly`
agent.

## Tool plugins

For tools that need a separate process, author an NCP plugin that executes capabilities and define the model-facing tools in Lua with `libs.tool`. `libs.compositors.tools.capability_tools_spec` wraps such a process as a tool source: it advertises the Lua definitions to `tool-gate`, validates each forwarded invocation, and sends `<name>.capability.invoke` to the plugin. Use [Plugin authoring](../../../docs/plugin-authoring.md) for transport and lifecycle, and see the bundled `basic-tools` and `git-worktree` plugins with `lua/libs/basic-tools` and `lua/libs/git-worktree` for the current shapes.

Tool advertisement, invocation, and approval are ecosystem protocols layered over NCP. Replacing `tool-gate` means owning their routing, provenance, permission, replay, and instruction-file integration.
