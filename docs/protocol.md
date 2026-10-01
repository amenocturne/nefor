# Current protocol

This document describes the protocol behavior shipped by the current Nefor Lua/plugin convention. It is not a frozen conformance spec.

## Boundary

The engine is a process spawner, raw line router, and Lua host. It does not parse NCP envelopes for routing, own sessions, choose a session root, or write an on-disk session log. The active composition selects the session root and wires persistence/resume through `lua/libs/sessions`. Plugin lines enter Lua through `core.ncp`; Lua decides whether a line is handshake traffic, a bus event, a direct delivery, or a dropped/error case.

Rust code still shares protocol-adjacent newtypes such as plugin names and timestamps, and Rust plugins may use nefor-protocol codecs. The shipped engine bus semantics are Lua-owned.

## Transport

Plugins are OS processes connected over stdin/stdout. Stdout lines from a plugin are complete JSON values encoded as JSON Lines. Stderr is logging, not bus traffic.

The runner starts `command[0]` directly with `std::process::Command`; it does not invoke a shell, set per-plugin cwd, manage per-plugin env maps, discover a plugin directory, inspect distribution inventory, or attach installation provenance. Children inherit the engine process cwd and environment. The engine exports resolved `NEFOR_CONFIG_DIR` and `NEFOR_DATA_DIR` before spawning. Lua registers the exact executable command; Lua/distribution code owns executable and immutable-runtime resolution, including packaged offline closure.

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

All three paths normalize one optional final LF and reject remaining literal LF. The maximum is `16 * 1024 * 1024` UTF-8 wire bytes, including the one emitted LF. JSON-escaped `\\n` remains ordinary content; the engine does not parse JSON for admission. Rejected `send` publishes nothing. `deliver_batch` validates every member before any enqueue: a later invalid member admits none. Atomic admission is not atomic writing, peer consumption, callback effects, or execution.

Bindings raise admission/enqueue errors usable with `pcall`. `nefor.engine.error_kind(err)` classifies the returned error as `frame_too_large`, `invalid_framing`, `unavailable`, or `writer_closed` (nil for other errors). A missing or disconnected delivery target is an error, not acknowledged loss. `send` is publication, not a delivery acknowledgement: its optional target is routing metadata consumed by Lua, and may identify an in-VM actor.

Rust SDK producers use `nefor_plugin_sdk::checked_send(&sender, message).await`, returning typed `TransportError::FrameTooLarge { bytes, limit_bytes }`, `InvalidFraming`, or `WriterClosed` before enqueue. Producers own small correlated rejection responses; the transport does not infer application IDs. Successful enqueue does not promise OS write or peer consumption. The stdout writer defensively closes on unchecked invalid sends. The bounded stdin reader ends after one size, invalid-UTF-8, or I/O error, without interpreting the oversized remainder as a new frame. A final unterminated EOF frame is still accepted. Malformed bounded JSON is a parse error, not transport death; readiness ignores parse errors but returns terminal transport faults.

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

Ordinary live writer-queue overflow still drops the oldest queued live line. Rust coalesces the lost count into `engine.plugin_delivery_lost { plugin, count, reason = "queue_overflow" }`, attributed to the broker-established connection identity. No application correlation is inferred from the dropped payload; accepted effects may be unknown at the affected capability scope. Finite replay batches never enter drop-oldest selection and retain their order. The upstream writer channel and bus log remain unbounded: the internal live queue cap is not a global memory bound. Frame admission likewise does not bound serialization or value-construction allocations.

A framing/read/write/flush fault makes only the failing connection unavailable, closes its I/O tasks, and emits exactly one `engine.plugin_transport_failed { plugin, phase, reason }` before requesting process termination. `phase` is `read`, `write`, or `flush`; `reason.kind` is `frame_too_large`, `invalid_utf8`, `read_io`, `write_io`, or `flush_io`. Oversize includes `reason.limit_bytes`. Rust does not parse tool/run/chat IDs. An idle broker still advances the local 2000ms close grace, then forcibly reclaims the runner-owned process group if necessary. In-memory transports produce no invented process fact.

The eventual OS outcome remains a distinct `engine.plugin_process_terminated` fact: `clean_exit`, `exit_code` (with `code`), `signal` (with `signal`), `crash`, `spawn_failure`, or `unknown`. Pending I/O notices are retained before OS accounting, so a transport fault precedes that process fact and never overwrites its actual outcome. Natural exit need not invent a transport fault. Transport teardown does not itself command global shutdown.

The engine does not synthesize an NCP shutdown system message. Rust reports one `engine.plugin_process_terminated` lifecycle fact for each spawned process termination, carrying the composition-assigned plugin identity and a typed outcome. Reporting the fact does not itself initiate shutdown. Lua composition decides whether and when to call `nefor.engine.shutdown { code, reason, grace_ms }`; the first request owns the shutdown fields and the single cooperative grace window. The starter interactive composition requests shutdown with `grace_ms = 2000` for every termination fact, preserving its cascade behavior; its CLI leaves request-terminal and persistence-flush teardown to the frontend owner. Plugins that want to announce departure may publish an ordinary plugin-authored goodbye event before exiting.
