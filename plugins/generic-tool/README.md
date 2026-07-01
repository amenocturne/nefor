# generic-tool

NCP v0.1 plugin: type-registry hub for the canonical tool protocol.
Sibling of `generic-provider` for the tool-execution role.

Owns two canonical types:

- `generic-tool.ToolCalls` -- list of tool invocations a provider asked for
- `generic-tool.ToolResults` -- list of tool execution outcomes that feed
  back into the provider on the next firing

On startup, registers these types via MAG's compile-time routing. Concrete tool
sources (basic-tools, mock-plugin, ...) separately declare `Into`/`From`
conversions against these tags. This plugin does not execute tools -- it is
a passive hub that makes the canonical type tags exist on the wire.
