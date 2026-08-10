# Nefor documentation

Nefor is a Lua-composable runtime. The engine is deliberately small: a user's
configuration assembles process plugins, tools, providers, workflows,
persistence, and interfaces into the system they run. The bundled starter is a
credential-free agentic TUI and the main example of that composition model.

## Use the starter

- [Getting started](user/getting-started.md) — install the starter and run its
  credential-free mock workflow.
- [Installation](user/installation.md) — install channels, platform artifacts,
  filesystem paths, updates, and config replacement.
- [TUI](user/tui.md) and [commands and keys](user/commands-and-keys.md) — the
  starter interface and its complete interaction reference.
- [Providers](user/providers.md) — enable ChatGPT, Ollama, or another
  OpenAI-compatible endpoint.
- [Workflows and tools](user/workflows-and-tools.md) — queueing, steering, run
  inspection, termination, files, images, receipts, and links.
- [Sessions and context](user/sessions-and-context.md) — persistence, resume,
  compaction, and context accounting.
- [Permissions](user/permissions.md) — safe, auto, and yolo across ordinary
  tools, write-review, and MAG human approval.
- [Troubleshooting](user/troubleshooting.md) — diagnose startup, plugin,
  provider, and terminal symptoms.

## Author MAG workflows

- [Orchestrating](mag/orchestrating.md) — choose MAG, author a graph, and run it.
- [Authoring reference](mag/authoring-reference.md) — language, graph, type, and
  artifact contracts.
- [Patterns](mag/patterns.md) — ready agents, parallel fan-in, shell pipelines,
  products, unions, and cycles.
- [Errors](mag/errors.md) — compiler, validation, runtime, and agent failures.

The [MAG plugin README](../plugins/mag/README.md) links the current actor model,
IR, lowering, and runtime pattern documentation.

## Customize a distribution

- [Configuration and composition](customization/configuration.md) — supported
  settings, composition APIs, and internal boundaries.
- [Providers and tools](customization/providers-and-tools.md) — register
  providers, tool sources, and permission policy.
- [Chat extensions](customization/chat-extensions.md) — extend the starter chat
  without copying its mechanism.
- [Distribution](customization/distribution.md) — package and maintain a
  complete composition.
- [Plugin authoring](plugin-authoring.md) — build a process plugin.

The [starter README](../starter/README.md) and [plugin index](../plugins/README.md)
document the shipped components beside their code.

## Current architecture and reference

- [Architecture](architecture.md) — the four execution layers and ownership
  boundaries.
- [Manifesto](manifesto.md) and [principles](principles.md) — normative product,
  architecture, and writing commitments.
- [CLI](reference/cli.md) — installed engine commands, flags, environment, and
  development-only surfaces.
- [NCP](reference/ncp.md) and [detailed protocol](protocol.md) — wire reference
  and current behavior.
- [nefor-pm](reference/nefor-pm.md) — package specifications, locking, checkout
  resolution, and runtime generations.
- [Session provenance](session-provenance.md) — how sessions identify the
  distribution that created them.
- [Glossary](glossary.md) — project terminology.
