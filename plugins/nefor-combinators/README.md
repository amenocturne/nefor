# nefor-combinators

NCP v0.1 plugin: combinator registry, signature query, and runtime
invocation.

Plugins register type-aware trait implementations (`Merge`, `Into`,
`Fanout`, `Equivalent`) via `combinators.register`. Callers query
resolvability (`combinators.query`) and invoke operations
(`combinators.invoke`) -- the plugin dispatches to the matching handler.

## Wire surface

- `combinators.register` -- plugins declare trait implementations
- `combinators.query` / `combinators.query.result` -- "do these signatures resolve?"
- `combinators.invoke` / `combinators.invoke.result` -- typed-multiset invocation
- `combinators.run` / `combinators.result` -- legacy path (migration period)
- `combinators.error` -- failure with a closed `ErrorCode`

## Run

Spawned by the engine over stdio. See `starter/init.lua` for the spawn
block.
