# Configuration and composition

Nefor has no engine-owned configuration schema. A configuration is a Lua program whose `init.lua` composes subprocess plugins and in-process actors. The shipped [`examples/nefor-agent/`](../) is a complete distribution, not a set of defaults the engine secretly applies.

Use these extension levels in order:

1. Change values in `config/init.lua`.
2. Add an actor through a supported builder or compositor.
3. Replace `init.lua` when the distribution's wiring must change.
4. Replace the starter entirely when the interface or runtime model changes.

The first two are supported customization seams. The latter two own all coupling the starter previously handled: package roots, actor ordering, readiness, provider translation, tool policy, sessions, MAG, and UI wiring.

## Select a configuration

```sh
nefor --config /path/to/config
# or
NEFOR_CONFIG_DIR=/path/to/config nefor
```

`--config` wins over `NEFOR_CONFIG_DIR`, which wins over the XDG default. See the [CLI reference](../../../docs/reference/cli.md).

A config-local module takes precedence over shared runtime Lua because the starter adds its own directory to `package.path` first. This is useful for config-owned modules, but the canonical chat mechanism deliberately loads fixed shared modules; use the [chat extension API](chat-extensions.md) rather than shadowing them.

## Starter settings

The starter's `config/init.lua` returns `{ active = ... }`. A practical customization starts from the complete shipped file and changes only the required values:

```lua
local M = {}

M.bin = function(name)
  local root = assert(os.getenv("NEFOR_EXECUTABLE_ROOT"), "NEFOR_EXECUTABLE_ROOT is required")
  return root .. "/" .. name
end

M.active = {
  default_provider = "mock-plugin",
  default_model = "mock-model",
  default_reasoning_effort = "xhigh",
  lead_reasoning_effort = "xhigh",

  providers = {
    { kind = "mock", name = "mock-plugin", mock_script = "mock-provider/init.lua" },
    {
      kind = "openai",
      name = "local",
      base_url = "http://localhost:11434",
      static_token = "ollama-local",
      extra_args = {},
    },
  },

  orchestration_profiles = {
    fast = { provider = "local", model = "model-name", reasoning_effort = "low" },
    deep = { provider = "local", model = "model-name", reasoning_effort = "high" },
  },

  tool_gate = {
    default_action = "prompt",
    auto_tools = { "read_file", "read_image", "mag", "mag-eval" },
    prompt_tools = {},
  },

  chat_extension = "my-chat-extension",
}

return M
```

`providers` in the shipped starter accepts descriptor kinds `mock`, `openai`, and `chatgpt`. OpenRouter is an `openai` instance, not a fourth provider kind; its request additions and usage extraction semantics live in that instance's provider table. A genuinely different provider kind is not a data-only setting: add its compositor call in `init.lua`. `usage.command_ids` and `usage.statusline_ids` independently choose the exact provider-owned values the example surface renders. `usage.account_ids` remain visible across an in-process session switch; `usage.session_ids` are cleared until their owner reconstructs or reports the new session. Likewise, prompt startup waits for a starter-owned inventory; changing actors may require changing its readiness declaration.

`log_level` exists in the current starter settings but has no runtime consumer. Do not rely on it as a behavior switch.

## Composition root

[`examples/nefor-agent/init.lua`](../init.lua) is the executable example. Its load order is significant:

1. resolve a mutable development tree, immutable runtime generation, or managed checkout;
2. register module roots with `nefor-pm` and establish `package.path`;
3. install the global `dispatch` hook;
4. create shared session, conversation, provider, MAG, workflow, tool, and UI actors;
5. initialize or resume session state and register actors in dependency order;
6. when `--prompt` is supplied, submit it only after the starter's readiness conditions are met.

Lua actors are registered with `core.actor`; subprocess wrappers are spawned through `core.ncp`. Tool sources must exist before the tool gate announces readiness so their one-shot advertisements are observed.

## Config-author compositor APIs

These APIs build actor or wrapper specs for the current starter contracts. They are not subprocess wire requirements.

### Providers

```lua
local provider = require("libs.compositors.provider")

local spec = provider.spawn_spec("local", {
  require("config").bin("openai-provider"),
  "--name", "local",
  "--base-url", "http://localhost:11434",
}, {
  translator_lib = "openai-provider",
  static_token = "ollama-local",
  agentic_loop = agentic_loop,
  conversations = conversation_reader,
  hooks = {
    intercept_inbound = function(env, helpers)
      -- Optionally publish, translate, or drop an inbound provider event.
    end,
    intercept_to_plugin = function(env)
      -- Optionally observe or suppress an outbound provider command.
    end,
  },
})
```

The provider compositor is intentionally coupled to Nefor's canonical provider/conversation contract. The translator must implement the full interface consumed by [`provider.lua`](../../../lua/libs/compositors/provider.lua); `agentic_loop` and `conversations` are required, replay is handled at the boundary, and conversation history remains manager-owned. Starter provider descriptors do not expose custom hooks or translator modules, so using those seams requires editing composition.

### Tools and chat bridge

```lua
local tools = require("libs.compositors.tools")
local chat_bridge = require("libs.compositors.chat_bridge")

local gate = tools.gate_spec("tool-gate", { config.bin("tool-gate") })
local policy = require("libs.model-context-policy")
local basic = tools.basic_actor_spec { max_read_bytes = policy.item_limit }
local worktrees = tools.git_worktree_actor_spec()
local bridge = chat_bridge.spawn_spec({ config.bin("nefor-tui"), chat_root })
```

These are convenience builders for the shipped distribution, not generic adapters. The tool builders fix canonical names and gate wiring. Replacing the gate also takes ownership of provenance and instruction-file behavior. The chat bridge preserves replay-bearing batches atomically and currently uses the fixed `nefor-tui` peer identity.

## What is internal

Do not build external configs against underscored actor fields, test reset hooks, broker dispatch helpers, or MAG kernel factories/registry tables. In particular, the MAG factory registry, run context, inventory, and router are implementation APIs. Author MAG programs through the language and artifact contracts documented by the MAG plugin; do not inject Lua factories into its embedded kernel.

For the process boundary, use [NCP](../../../docs/reference/ncp.md). For distribution replacement, see [Distribution](distribution.md).
