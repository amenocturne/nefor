# mag (plugin)

Hosts the MAG runtime: loads `.mag` programs (via `crates/nefor-mag`) into a
resident evaluator and runs them as constellations of lightweight in-memory
actors, folding graph modifications over an initially empty graph. The lead
writes tiny MAG programs ad-hoc during sessions; this plugin turns them into
running workflows.

## Kernel

The plugin ships its own kernel Lua tree at `lua/mag-kernel/` (entry
`lua/mag-kernel/init.lua`: actor fold, factories, routing, run contexts,
observer stream). The embedded VM loads it at startup — configs no longer
carry a kernel copy. Resolution order, highest precedence first:

1. `--kernel <path>` (or `-k`) — explicit override for dev experiments and the
   plugin's integration tests.
2. `<lua-root>/../plugins/mag/lua/mag-kernel/init.lua` — the default. The
   composition threads `--lua-root` (`NEFOR_ROOT/lua`); its parent is
   `NEFOR_ROOT`, which carries the whole `plugins/` tree in every install mode
   (dev checkout, `NEFOR_LOCAL_DIR` override, or the pm sparse-clone whose cone
   includes `plugins`). No packaging step copies the kernel — it rides the same
   tree the shared Lua libs do.
3. `NEFOR_DEV_DIR/plugins/mag/lua/mag-kernel/init.lua` — in-checkout dev
   fallback when no `--lua-root` is passed.

## Docs

- [../../docs/architecture.md](../../docs/architecture.md) — the four execution layers and what lives where
- [docs/actor-model.md](docs/actor-model.md) — actors, factories, lifecycle, contracts, signals
- [docs/ir.md](docs/ir.md) — graph modifications, the fold, firing, rules, application semantics
- [docs/lowering.md](docs/lowering.md) — MAG graph syntax → modification: edges into routes, namespacing, shell defaults
- [docs/patterns.md](docs/patterns.md) — canonical shapes for MAG programs (dependencies, joins, races, retries)
