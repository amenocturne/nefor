# mag (plugin)

Hosts the MAG runtime: compiles `.mag` programs (via `crates/nefor-mag`) into
immutable versioned inline envelopes and runs them as constellations of
lightweight in-memory actors, folding graph modifications over an initially
empty graph. The lead
writes tiny MAG programs ad-hoc during sessions; this plugin turns them into
running workflows.

## Tool projection

Agent `tools` lists are requested capability profiles, not schema registries.
MAG snapshots the configured tool gate's owner-qualified `tool.register`
advertisement and attaches the matching descriptors to each provider request.
Providers consume that request-local snapshot, while the original names remain
the immutable allowlist enforced when a model invokes a tool. Names absent from
the runtime advertisement are omitted from the model surface instead of
failing the delegated run.

Direct providers attach `model` and `duration_ms` to terminal observations and
to usage observations when token usage exists. MAG projects those fields
unchanged, so the chat surface can render per-turn model, elapsed time, and
throughput without knowing which provider produced the response.

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

- [MAG inside Nefor](<../../mag/book/02. nefor/README.md>) — MAG as applied inside Nefor
- [../../docs/architecture.md](../../docs/architecture.md) — the four execution layers and what lives where
- [docs/actor-model.md](docs/actor-model.md) — actors, factories, lifecycle, contracts, signals
- [docs/ir.md](docs/ir.md) — program/delta envelopes, the fold, firing, operations, application semantics
- [docs/lowering.md](docs/lowering.md) — MAG graph libraries → modification: edges into routes, namespacing, shell defaults
- [docs/patterns.md](docs/patterns.md) — canonical shipped shapes for MAG programs (dependencies, joins, cycles, failure repair, fanout/timeouts)

## Explicit project builds

`mag.load` remains the cold, manifest-free compile request. `mag.build` is an
additive opt-in request; neither request starts actors. Both reply with
`mag.loaded` or `mag.error`, correlated by `in_reply_to` to the request `id`.

```json
{
  "kind": "mag.build",
  "id": "compile-1",
  "project_root": "/installed/config",
  "entry": "agentic-loop/lead-turn.mag",
  "module_roots": ["/installed/runtime/mag/lib", "mag/lib"],
  "cache_dir": "/writable/data/mag/cache",
  "no_cache": false
}
```

Project and cache roots must be explicit absolute paths. Entry must be nonempty
and relative, without `..` components. Extra module roots are optional ordered
nonempty paths; relative roots are anchored to the project. Shared compiler
preparation reads exactly `project_root/mag.toml` and constructs roots in order:
project, manifest roots, request roots (duplicates retained). Missing or invalid
manifests fail normally. No parent discovery or source staging occurs.

`mag.loaded` retains its normal `artifact`, `hash`, `factories`, and
`factory_contracts` fields. Build replies additionally contain:

```json
{
  "build": {
    "status": "hit",
    "lookup_duration_ns": 123,
    "publication_duration_ns": 0
  }
}
```

Status is `hit`, `miss`, `bypass`, or `unavailable`. `no_cache` defaults false;
true bypasses both lookup and publication. Unavailable identity disables caching;
failed storage lookup/publication instead reports `miss` and preserves successful
cold compilation. Diagnostics are outside the artifact and never affect its hash.
Errors retain the existing error reply shape, without build diagnostics.

Every build fetches fresh factory contracts. Their object keys are sorted at the
Lua-to-JSON boundary because Lua table iteration order is process-local; contract
array ordering and all values remain intact. The cache identity includes this
complete host input, original project root, relative entry, effective ordered
roots, manifest version, compiler options, and bytes of the running **mag-plugin**
executable. Live execution model snapshots are not compile inputs. Cache placement
is not identity; storage can persist independently of read-only installed source.

Hits decode the exact successful compiler bytes with a depth-safe JSON Value
parser and then use the same envelope, hash, kernel, and provider-schema preflight
as cold loads and misses. A compiler success can be cached even when Nefor rejects
its envelope. A hit is never proof of current kernel acceptance. Corrupt records
fall back to compilation. No execution state, model snapshot, source environment,
or failure is retained in this cache. Execute/apply and retained artifact lifetime
remain unchanged; deleting cache/source after loading does not dispose an artifact.

## Output admission and run settlement

Every outgoing envelope uses SDK checked admission (16 MiB including LF).
Admission success is enqueue only, not stdout delivery or peer acceptance.
Before execution begins, a rejected correlated compilation/error reply becomes
one bounded `mag.error {in_reply_to, message}`. Required uncorrelated startup or
control output rejection exits rather than advertising a usable authority.

An accepted run is never turned into a pre-start `mag.error`. A rejected
capability request is delivered to its exact kernel correlation as a failure;
normal graph failure routing determines whether that firing's failure is handled.
The bridge forgets a rejected provider correlation before that delivery. Kernel
bus emissions carry explicit run identity, not identity parsed from actor names.

Rejected run lifecycle/output observations mark the owning kernel context for
output failure. Its remaining output progression stops, and settlement reaps its
actors (including parallel capability cancellation) and drops the run context.
Exactly one execute-correlated `mag.run_result` has `status = "failed"`,
`failure = "output_delivery"`, and a bounded error explaining that earlier or
external effects may have occurred. A rejected oversized terminal result uses
that same small terminal instead of publishing success. Persisted output files
are not removed; `persisted` and `output_path` evidence are retained when available.
This is failure to deliver output, not proof that completed execution effects
failed or never happened. No execution is retried.

If identity cannot be resolved to a surviving owner, cancellation/teardown
observation admission fails, the small fallback cannot fit, or the writer closes,
the plugin tears down local kernel authority and exits. Downstream owners must
reconcile accepted work as unknown; they must not invent effects-free rejection.
Writer closure and terminal stdin faults wake the idle loop. Bounded malformed
JSON remains parse noise. Goodbye is checked best effort only during already
selected teardown. These policies do not promise atomic OS delivery or bound
value-construction/serialization memory.
