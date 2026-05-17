# core

NCP v0.1 protocol implementation in Lua. Provides the primitives every bus consumer depends on: envelope construction, NCP protocol state machine, actor runtime, ID minting, and history-replay helpers.

Shipped as a library, not a plugin. Require as a bundle (`require("core")`) or granularly (`require("core.envelope")`, `require("core.ncp")`, `require("core.actor")`, etc.).

Independent generic libs live under `lua/libs/`; plugin-specific helpers live alongside their plugin.
