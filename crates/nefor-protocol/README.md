# nefor-protocol

Protocol-adjacent Rust types and JSON Lines codecs for NCP-shaped envelopes.

Rust plugins and tests can use this crate to produce and consume the conventional envelope shapes without reimplementing the codec. The current shipped engine uses shared newtypes such as PluginName and Timestamp, but delegates NCP envelope semantics and bus behavior to Lua (lua/core/ncp.lua); it is not a Rust-side strict-envelope router.

See the current protocol behavior in ../../docs/protocol.md.
