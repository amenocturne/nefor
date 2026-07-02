# mag (plugin)

Hosts the MAG runtime: loads `.mag` programs (via `crates/nefor-mag`) into a
resident evaluator and runs them as constellations of lightweight in-memory
actors, folding graph modifications over an initially empty graph. The lead
writes tiny MAG programs ad-hoc during sessions; this plugin turns them into
running workflows.

## Docs

- [../../docs/architecture.md](../../docs/architecture.md) — the four execution layers and what lives where
- [docs/actor-model.md](docs/actor-model.md) — actors, factories, lifecycle, contracts, signals
- [docs/ir.md](docs/ir.md) — graph modifications, the fold, firing, rules, application semantics
- [docs/patterns.md](docs/patterns.md) — canonical shapes for MAG programs (dependencies, joins, races, retries)
