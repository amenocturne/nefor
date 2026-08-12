# Lua libraries

Reusable composition mechanisms shipped with Nefor. A distribution requires the
libraries it needs and supplies its own wiring and policy; the installable
reference composition lives in [`examples/nefor-agent`](../../examples/nefor-agent/README.md).

The current ownership map and placement guidance are in
[Architecture](../../docs/architecture.md). In particular,
`conversation-manager` owns canonical conversation facts, model context, and
provider-neutral projections, while `agentic-loop` owns transient turn
orchestration and queueing.

Library-specific public seams are documented alongside their implementation
when they need more than the architecture guide. See, for example, the
[headless CLI surface](cli/README.md).
