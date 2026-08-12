# Nefor documentation

Nefor is a runtime for arbitrary Lua-owned compositions. The engine is a pure
string bus: it spawns processes, hosts Lua, routes lines through the configured
dispatch hook, and stamps transport identity. Protocols, persistence, agents,
interfaces, providers, and tools are composition or plugin concerns.

## Runtime

- [Manifesto](manifesto.md) — product boundary and design commitments.
- [Architecture](architecture.md) — execution layers and ownership.
- [Principles](principles.md) — architecture and writing rules.
- [CLI](reference/cli.md) — generic engine options, paths, and config-owned
  forwarded arguments.
- [NCP convention](reference/ncp.md) and [current behavior](protocol.md) — the
  Lua/plugin convention layered over the string bus.
- [Plugin authoring](plugin-authoring.md) — subprocess contract and composition
  wrappers.
- [Session provenance](session-provenance.md) — distribution identity for
  persisted sessions.
- [Glossary](glossary.md) — project terminology.

## Explore and compose

- [Nefor agent example](../examples/nefor-agent/README.md) — an explorable,
  installable composition demonstrating a TUI, providers, tools, sessions,
  permissions, and MAG orchestration. It is an example, not the product.
- [Plugin index](../plugins/README.md) — plugin capabilities and local docs.
- [Lua core](../lua/core/README.md) and [Lua libraries](../lua/libs/README.md) —
  reusable composition mechanisms.
- [nefor-pm](../lua/nefor-pm/README.md) — package, checkout, lock, and runtime
  generation reference.
- [MAG compiler](../crates/nefor-mag/README.md) — language, compiler, and error
  reference.
- [MAG runtime plugin](../plugins/mag/README.md) — actor kernel, lowering, and
  canonical runtime patterns.
- [TUI plugin](../plugins/nefor-tui/README.md) — generic declarative terminal UI
  capability.
