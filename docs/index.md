# Nefor documentation

Documentation for **Unreleased** at `505a764`, following v0.4.0.

Nefor is a Lua-composable runtime. The engine is deliberately small: a user's
configuration assembles process plugins, tools, providers, workflows,
persistence, and interfaces into the system they run. The bundled starter is a
credential-free agentic TUI and the main example of that composition model.

## Use Nefor

- [Getting started](user/getting-started.md) — install the starter, launch it
  with the offline mock, and learn the first useful controls.
- [Installation](user/installation.md) — supported install channels, platform
  artifacts, filesystem paths, updates, and config replacement.
- [Troubleshooting](user/troubleshooting.md) — diagnose startup, plugin,
  provider, and terminal symptoms.
- [Approval model](approval-model.md) — what safe, auto, and yolo modes permit.
- [Session provenance](session-provenance.md) — how sessions identify the
  distribution that created them.

## Understand and extend Nefor

- [Manifesto](manifesto.md) — the project's product and design commitments.
- [Architecture](architecture.md) and [principles](principles.md) — execution
  layers and placement rules.
- [Glossary](glossary.md) — project terminology.
- [Plugin authoring](plugin-authoring.md) — build a process plugin.
- [Protocol](protocol.md) — current NCP wire behavior.
- [Testing](testing.md) — repository test strategy and commands.

Component-specific references live beside their code, including the
[starter composition](../starter/README.md), [plugins](../plugins/README.md),
and [MAG](../plugins/mag/README.md).
