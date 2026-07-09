# Retired NCP v0.1 draft

The versioned v0.1 spec draft no longer describes the shipped engine. The current implementation is a Lua-owned NCP/string-bus system: the Rust engine routes raw lines and invokes Lua, while lua/core/ncp.lua owns handshake, event publishing, replay, errors, and wrapper delivery.

Use the current protocol document instead: ../../docs/protocol.md.
