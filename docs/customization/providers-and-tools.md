# Providers, tools, and permission policy

Providers and tools cross several independent boundaries: process transport, canonical event translation, model advertisement, capability allowlists, and human permission policy. Adding a binary is therefore not always enough; preserve every boundary the new component participates in.

## Providers

The starter supports three provider descriptor kinds:

- `mock` for deterministic local scenarios;
- `openai` for an OpenAI-compatible endpoint;
- `chatgpt` for the bundled ChatGPT provider.

Configure those through [`config.active.providers`](configuration.md#starter-settings). A new provider kind requires composition code. Prefer `libs.compositors.provider.spawn_spec` when the provider can satisfy the canonical translator interface; see [Configuration and composition](configuration.md#providers).

A provider subprocess speaks NCP, but provider request/history/stream types are a higher-level contract. `generic-provider` owns canonical type declarations and concrete providers translate to them. Do not treat those event kinds as NCP itself.

## Config-defined read-only tools

`libs.read-only-tools` exposes a supported registration seam:

```lua
return require("libs.read-only-tools").build {
  include = {
    "list_dir",
    "search_text",
    "instructions",
    "discover_instruction_files",
    "skill",
  },
  extra_tools = {
    {
      schema = {
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
          label = "My read tool",
          primary = { arg = "query" },
          result = { kind = "content" },
        },
      },
      handler = function(args, emit)
        emit.ok("result for " .. args.query)
        -- On expected failure, call emit.err("message") instead.
      end,
    },
  },
}
```

Base tools are opt-in: `build {}` advertises none. `python-read` is currently an unavailable placeholder. Extra schemas require Nefor's `display` metadata in addition to JSON Schema. Handlers should call exactly one emitter and results are stringified. All tools built here share source identity `read-only-tools`.

Spawn the actor before `tool-gate`, which triggers its one-shot advertisement. A tool intended for delegated agents may also need entries in the MAG toolset/allowlist, the validator's read-only inventory, gate policy, and startup readiness.

## Permission policy

Registration makes a tool available; it does not authorize every invocation. `libs.tool-validator` classifies requests after validating the invoking agent's capability allowlist.

```lua
return require("libs.tool-validator").build {
  read_only_tools = { "read_file", "my_read_tool" },
  auto_approve_tools = { "my_schema_limited_safe_tool" },
  bash_fastpaths = {
    function(command, read_only)
      return not read_only and command:match("^my%-cli%s+read%s") ~= nil
    end,
  },
}
```

Policy precedence is:

1. validate that capability data is well-formed and includes the requested tool;
2. apply `yolo` mode;
3. apply `auto_approve_tools`;
4. apply special `edit_file`, `write_file`, and `bash` rules;
5. approve other tools when the entire capability allowlist is read-only;
6. prompt in `safe` or deny in `auto`.

Malformed or excluding capability data fails closed even in `yolo`. A missing allowlist retains legacy unrestricted behavior. `auto_approve_tools` is unconditional after capability validation, so use it only for schemas whose complete input space is safe. `bash_fastpaths` are trusted policy predicates and run before `da`.

`edit_file` is available only to write-capable agents. `write_file` additionally requires an active approved lead-workflow plan except in `yolo`. Non-read-only `bash` uses `da`; a missing classifier is an installation error, not a prompt fallback. See the full [approval model](../approval-model.md).

The starter currently derives `read_only_tools` from `mag/lib/nefor/toolsets.json`. Custom `auto_approve_tools` and fast paths are source-exposed composition seams, but have less focused regression coverage than the base mode behavior.

## Tool plugins

For tools that need a separate process, author an NCP plugin and advertise to `tool-gate`. The gate forwards invocations to the advertising source and correlates `tool.result`. Use [Plugin authoring](../plugin-authoring.md) for transport and lifecycle, and inspect the bundled `basic-tools` and `git-worktree` plugins for current higher-level event shapes.

Tool advertisement, invocation, and approval are ecosystem protocols layered over NCP. Replacing `tool-gate` means owning their routing, provenance, permission, replay, and instruction-file integration.
