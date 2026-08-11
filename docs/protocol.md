# Current protocol

This document describes the protocol behavior shipped by the current Nefor Lua/plugin convention. It is not a frozen conformance spec.

## Boundary

The engine is a process spawner, raw line router, and Lua host. It does not parse NCP envelopes for routing and it does not own sessions or an on-disk session log. Plugin lines enter Lua through core.ncp; Lua decides whether a line is handshake traffic, a bus event, a direct delivery, or a dropped/error case.

Rust code still shares protocol-adjacent newtypes such as plugin names and timestamps, and Rust plugins may use nefor-protocol codecs. The shipped engine bus semantics are Lua-owned.

## Transport

Plugins are OS processes connected over stdin/stdout. Stdout lines from a plugin are complete JSON values encoded as JSON Lines. Stderr is logging, not bus traffic.

The runner starts command[0] directly with std::process::Command; it does not invoke a shell, set per-plugin cwd, or manage per-plugin env maps. Children inherit the engine process cwd and environment. The engine resolves and exports NEFOR_CONFIG_DIR, NEFOR_DATA_DIR, and NEFOR_PLUGIN_DIR before spawning so Lua configs and child processes see the same roots.

## Plugin-authored lines

The common plugin line shape is a JSON object with type set to system or event and body set to an object containing a kind.

System lines are consumed by the Lua NCP framework. Event lines are plugin speech. Event body must be a JSON object.

Current Lua accepts a plugin-supplied from field on event lines; if absent, it uses the source connection name. Plugins in the shipped ecosystem should not depend on spoofing or overriding sender identity. Treat plugin-authored from as compatibility behavior, not an authority model.

## Ready handshake

A plugin normally sends a system ready body with protocol_version 0.1. Lua replies directly to that plugin with ready_ok, from engine, a timestamp, and engine_version 0.1.0.

Current checks are intentionally small: protocol_version must be the string 0.1, and duplicate ready messages are rejected. Extra ready-body fields are not rejected by the shipped Lua framework. There is no shipped ready timeout.

Events sent before readiness are rejected with an error direct-delivered to the source plugin.

## Bus publishing and delivery

Lua exposes three engine paths:

- nefor.engine.send(payload, target?) publishes a Step entry to the in-memory bus log. The broker drain invokes Lua dispatch for the new tail.
- nefor.engine.deliver(peer, payload) writes one line to one peer stdin without appending a bus-log entry.
- nefor.engine.deliver_batch(peer, payloads) writes a finite ordered batch to one peer without appending bus-log entries or applying the ordinary live-queue overflow policy. Earlier deliveries drain first; later deliveries and connection close cannot overtake the batch. This path is intended for replay/state restoration, not unbounded live streams.

The bus log contains only payloads explicitly published with send. Session persistence is implemented by Lua session actors that observe the bus; the engine itself writes no session jsonl.

Default core.ncp routing offers published Step events to ready wrappers. Without a wrapper override, delivery:

- skips the source peer;
- respects a target passed to send;
- applies the legacy peer kind-prefix routing convention when the prefix names a ready peer;
- otherwise broadcasts to other ready peers.

Wrappers may override from_plugin(envs) and to_plugin(envs). These callbacks are batched and side-effecting: they receive a list of envelopes and publish or deliver by calling engine bindings. Their return values are ignored.

## Replay

When a plugin completes ready, core.ncp replays prior bus-log events to that plugin wrapper so late attachers can rebuild state. Replay is a Lua framework behavior over the in-memory engine log; long-term session resume is owned by lua/libs/sessions.

## Errors and shutdown

Malformed JSON, invalid envelope type, event body that is not an object, unknown system kind, duplicate ready, and protocol-version mismatch produce direct error messages from Lua to the source plugin.

Ordinary live writer-queue overflow drops the oldest queued live line and is logged by the engine; no shipped protocol-level queue_overflow message is emitted. Explicit finite replay batches are lossless and do not make the live queue unbounded.

The engine does not synthesize an NCP shutdown system message on normal peer departure. Shutdown is connection closure/cascade-close behavior coordinated by the engine and Lua composition. Plugins that want to announce departure may publish an ordinary plugin-authored goodbye event before exiting.
