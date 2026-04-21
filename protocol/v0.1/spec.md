# NCP — Nefor Composition Protocol

**Version:** 0.1 (draft)
**Status:** Working draft — not yet stable. May change without semver notice until v0.1 is ratified.

NCP is the communication protocol between the nefor engine and its plugins. It is the contract at nefor's layer-2 boundary: the engine is a reference implementation of this protocol, but any engine that implements NCP inherits the entire nefor plugin ecosystem.

This document specifies the protocol. The engine implementation, plugin manager, reference plugins, and frontend are documented elsewhere.

## 1. Overview

NCP defines what the engine sees. It does not define or limit what plugins do among themselves outside of NCP.

Specifically, NCP defines:

- A **message envelope** with two engine-stamped delivery facts (`from`, `ts`) and two plugin-authored fields (`type`, `body`).
- A binary split between **system messages** (bodies follow shapes defined by this spec) and **event messages** (bodies are opaque to the engine).
- **Broadcast semantics** — every plugin sees every bus message.

### Design principles

- **Minimal.** The engine understands only the system messages defined below. Everything else is opaque.
- **Broadcast.** The engine is a bus: every plugin sees every event. Plugins filter by matching on body fields.
- **Non-spoofable delivery.** `from` and `ts` are engine-stamped; a plugin cannot lie about sender identity or arrival time.
- **Reject, don't repair.** The engine validates every message and either delivers it unchanged or drops it with a named `error` back to the sender. It never silently corrects, reinterprets, or best-effort-forwards malformed input. Every fault has a code and a named recipient — plugins are never left guessing.
- **Engine narrates delivery; plugins narrate content.** The engine's vocabulary is the envelope plus the system message shapes. All other semantics are plugin speech.
- **Plugins are processes.** A plugin does whatever it wants inside its own process — spawn subprocesses, open files, make network requests, use whatever runtime it likes. NCP does not regulate this. The engine only manages the connection lifecycle and message brokering.
- **Sub-protocols emerge.** Plugins define their own message shapes under their plugin-name namespace. NCP does not register, arbitrate, or validate these.

## 2. Transport

- **One connection per plugin.** Each plugin speaks NCP over a pair of byte streams to the engine.
- **Default transport:** stdio. The engine spawns the plugin as a subprocess with stdin and stdout attached to the routing loop. Stderr is a log channel, not part of NCP.
- **Alternative transports:** Unix domain socket and TCP. Same wire format. Transport-deployment specifics (socket path, permissions, TCP binding, TLS) are engine-configuration concerns outside NCP.
- **Wire format:** JSON Lines (also called NDJSON). One complete JSON object per line, `\n`-terminated, UTF-8. The message itself is serialized compactly (no internal line breaks); the `\n` is purely the frame boundary between messages.
- **Line bound:** 16 MiB per line by default. Plugins needing to move larger payloads should arrange that outside of NCP.
- **Ready timeout.** The engine MUST close connections that do not send a valid `ready` as their first message within a bounded time. The exact bound is implementation-defined (10 seconds is the recommended default).

## 3. Envelope

Every NCP message is a JSON object with exactly these four fields:

```json
{
  "type": "system" | "event",
  "from": "<plugin-name>",
  "ts":   "<ISO-8601 UTC timestamp with millisecond precision>",
  "body": { … }
}
```

| Field  | Set by                      | Purpose                                                                                                            |
| ------ | --------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `type` | Plugin                      | `"system"` → body follows a shape defined in §5. `"event"` → body is plugin-authored and engine does not parse it. |
| `from` | Engine (assigned at spawn)  | Sender identity. Non-spoofable. Assigned from engine spawn-config; never sourced from any plugin-authored field.   |
| `ts`   | Engine (stamped on receive) | Global ordering timestamp.                                                                                         |
| `body` | Plugin                      | Content. MUST be a JSON object.                                                                                    |

**No envelope fields beyond `type`, `from`, `ts`, `body` are permitted.** All other data goes in `body`.

The engine enforces the envelope strictly. A message with missing required fields, extra fields, fields of incorrect type, or a body that is not a JSON object is **dropped** — it does not reach other plugins. The engine responds to the sender with a system `error` naming the specific violation (code `malformed_envelope` for envelope-level faults, `body_not_object` when `body` is present but of the wrong JSON type).

### Reserved `from` identity

`from: "engine"` is reserved. Plugins cannot be assigned the name `engine`. Messages stamped with `from: "engine"` are authored by the engine itself.

### Plugin-sent vs engine-broadcast envelopes

A plugin sends messages with exactly two envelope fields: `type` and `body`. The engine populates `from` (from the engine-assigned connection identity) and `ts` (receive-time wall clock) before broadcasting.

An outgoing message from a plugin that contains `from`, `ts`, or any field other than `type` and `body` is rejected per the enforcement rules above: the engine drops it and responds with a system `error` naming the violation. The engine does not silently correct the envelope; the plugin must fix its code.

Plugins consuming messages MUST trust the envelope `from` and `ts` on received messages. These are the engine's authoritative word.

## 4. Message types

### `type: "system"`

Body follows a shape defined in §5. The engine parses, validates, and acts on these.

### `type: "event"`

Body is plugin-authored. The engine does not parse body for event messages. Body MUST be a JSON object (not array, string, number, or null), but is otherwise unconstrained.

## 5. System messages

Every system body has a `kind` field drawn from the fixed vocabulary below. Engines MUST reject system messages with unrecognized `kind` values via a system `error` message with code `unknown_kind`.

The recognized kinds in v0.1:

### 5.1 `ready` — plugin → engine

First message a plugin sends after connecting. Declares the NCP protocol version the plugin speaks. The engine assigns identity (`from`) independently of this message — plugin-authored identity is not part of the wire.

```json
{
  "type": "system",
  "from": "plugin-name",
  "ts": "…",
  "body": {
    "kind": "ready",
    "protocol_version": "0.1"
  }
}
```

Body fields:

- `protocol_version` (string, required) — the NCP version the plugin implements, in [SemVer 2.0.0](https://semver.org) format or `MAJOR.MINOR` shorthand (e.g., `"0.1"`). The engine rejects the ready with `protocol_version_mismatch` if this does not match a version it supports (see §9 for negotiation policy). Structural faults (missing field, wrong type, extra fields) are rejected with `invalid_ready`.

### 5.2 `ready_ok` — engine → plugin

Accepts the ready handshake. Sent to the readying plugin only.

```json
{
  "type": "system",
  "from": "engine",
  "ts": "…",
  "body": {
    "kind": "ready_ok",
    "engine_version": "0.1.0"
  }
}
```

Body fields:

- `engine_version` (string) — the engine's version in [SemVer 2.0.0](https://semver.org) format. Sent as-is for plugins to consume however they see fit — common uses include logging, diagnostics, and engine-version-specific backward-compat handling. NCP does not constrain plugin use.

### 5.3 `shutdown` — engine → all

The engine is shutting down. It sends this to every connected plugin, then force-closes remaining connections after `grace_ms` milliseconds (or after an implementation-defined grace period if `grace_ms` is absent).

```json
{
  "type": "system",
  "from": "engine",
  "ts": "…",
  "body": { "kind": "shutdown", "reason": "user quit", "grace_ms": 2000 }
}
```

Body fields:

- `reason` (string, optional) — free-form explanation of why the engine is shutting down (user-initiated quit, signal received, crash recovery, etc.). Provided so plugins and operators can distinguish intended shutdowns from failures.
- `grace_ms` (integer, optional) — milliseconds between the engine sending `shutdown` and force-closing remaining connections. Bounds the window in which plugins can finalize before their connection is cut. If absent, the engine picks an implementation-defined grace period.

### 5.4 `error` — engine → one plugin

Reports a protocol-level error to the plugin that caused it. Covers both ready-phase failures (malformed ready, version mismatch) and runtime failures (malformed envelope, queue overflow, etc.). Whether the connection closes depends on the error code — see §8 for the per-code policy.

```json
{
  "type": "system",
  "from": "engine",
  "ts": "…",
  "body": {
    "kind": "error",
    "code": "malformed_envelope",
    "message": "body is not a JSON object",
    "offending": { "from": "plugin-name", "ts": "…" }
  }
}
```

Body fields:

- `code` (string) — machine-readable error identifier drawn from §8. Plugin code branches on this to decide how to react.
- `message` (string) — human-readable explanation, intended for plugin-side logs and operator diagnostics.
- `offending` (object, optional) — identifies the specific message that caused the error by its delivery facts (`from`, `ts`), so plugins can correlate the error with the send attempt that caused it. For rejected outbound messages, these are the facts the engine assigned (or would have assigned) before rejecting. Absent when the error cannot be attributed to a specific received message (e.g., connection-level framing errors).

## 6. Broadcast semantics

### Delivery

**The bus carries event messages only.** Every event message a plugin sends is broadcast to every _other_ connected plugin. The sender does not receive its own messages. This is the bus.

**System messages are never on the bus.** They flow point-to-point on the direct connection between the engine and individual plugins. Every system message has a specific recipient set determined by its kind:

| Direction                                 | Messages            | Seen by                                                                |
| ----------------------------------------- | ------------------- | ---------------------------------------------------------------------- |
| Plugin → engine                           | `ready`             | The engine only                                                        |
| Engine → one plugin                       | `ready_ok`, `error` | The addressed plugin only                                              |
| Engine → every currently connected plugin | `shutdown`          | Each recipient individually (engine delivers N times, not via the bus) |

A plugin's inbound stream mixes two flows — bus events and engine system messages — but they arrive via different mechanisms. The `ts` field gives a single global ordering across both.

Peers that care about when plugins join or leave the bus implement their own convention: a plugin-authored `hello` event after `ready_ok`, a periodic heartbeat, or a `goodbye` event before exit. These are ecosystem conventions documented in [`docs/plugin-authoring.md`](../../docs/plugin-authoring.md), not spec-level primitives.

### Ordering

The engine delivers messages to each plugin in a single global order — the order in which the engine stamped `ts`. Two plugins observing the same bus will see the same subsequence of `(from, ts, body)` tuples, modulo any messages dropped from their individual queues by backpressure (see below). Ordering is monotonic in `ts`.

### Backpressure

Each plugin has a bounded receive queue on the engine side (default capacity: 1024 messages, configurable per-plugin). When a plugin's queue is full:

- The engine drops the oldest queued message for that plugin.
- The engine emits a system `error` to that plugin with code `queue_overflow` identifying the dropped message's delivery facts.
- Other plugins' queues are unaffected.

The engine itself is never blocked by a slow consumer.

### Sender authority

See §3 "Plugin-sent vs engine-broadcast envelopes." `from` and `ts` on the wire are the engine's authoritative word.

## 7. Plugin contract

The complete list of what the engine requires from a plugin:

1. Speak the transport (§2).
2. Use the envelope (§3) for every message.
3. Send `ready` as the first message; the engine responds with `ready_ok` (success) or `error` (rejection — see §8 for codes).
4. Handle `shutdown` as defined in §5.3.
5. Emit only well-formed system messages (§5) when using `type: "system"`.
6. Signal departure by closing stdout / exiting. No farewell system message exists at the protocol level; peers that care about plugin liveness rely on ecosystem conventions (see §6).

That is the complete contract. Everything else a plugin does inside its own process — the language, the runtime, the concurrency model, subprocesses, filesystem and network access, any other systems it talks to outside of NCP — is outside the scope of this specification. NCP neither constrains nor cares.

Ecosystem conventions (kind namespacing, request/response patterns, addressed messages, naming style, hello/goodbye/heartbeat patterns) are not part of this spec — they live in the plugin authoring guide ([`docs/plugin-authoring.md`](../../docs/plugin-authoring.md)).

## 8. Errors

### Error codes

Codes used in `error.code`. The "closes" column indicates whether the engine closes the connection after sending the error.

| Code                        | Closes | Meaning                                                                                                |
| --------------------------- | :----: | ------------------------------------------------------------------------------------------------------ |
| `protocol_version_mismatch` |  yes   | Plugin's declared `protocol_version` is not supported by the engine                                    |
| `invalid_ready`             |  yes   | Ready body missing required fields, wrong types, or malformed version strings                          |
| `malformed_envelope`        |  no\*  | Received line is not valid JSON, or missing required envelope fields, or has forbidden envelope fields |
| `body_not_object`           |   no   | Envelope's `body` is not a JSON object                                                                 |
| `unknown_kind`              |   no   | System-typed message has a `kind` not defined by this spec                                             |
| `queue_overflow`            |   no   | Plugin's receive queue was full; engine dropped a message                                              |
| `rate_limited`              |   no   | Plugin exceeded per-connection rate limits                                                             |

\* `malformed_envelope` does not close the connection for ordinary JSON-field errors. Connection-level framing errors (invalid UTF-8, line exceeding the 16 MiB bound, JSON that cannot be parsed at all) cause the engine to emit `error` with code `malformed_envelope` once and then close — the plugin has proven it can't produce valid frames at all.

Plugins MAY define their own error-shaped events on the bus for their own sub-protocols. This spec only constrains the engine's error taxonomy above.

### Error handling

- Errors whose code closes the connection (see table) are followed by engine-side shutdown of the transport.
- Errors whose code does not close leave the connection open. The engine MAY escalate to `rate_limited`, and ultimately to eviction, for connections that repeatedly generate close-triggering or malformed messages.

## 9. Versioning

NCP uses semver on the protocol itself (distinct from any implementation's version).

- **Major** — incompatible change to envelope, transport, or any system message shape.
- **Minor** — additions: new system `kind` values, new error codes.
- **Patch** — editorial clarifications only.

### Version negotiation (v0.1)

In v0.1, engine and plugin MUST agree on protocol version `"0.1"` exactly. A mismatch causes `error` with `code: "protocol_version_mismatch"` and closes the connection. Until v0.2 exists, a v0.1 engine rejects any `protocol_version` other than `"0.1"`, regardless of engine implementation version.

Future versions will introduce negotiation (engine declares supported range, plugin picks the highest mutually supported version in `ready`). This is deliberately deferred until there is a second version.

## 10. Conformance

### Engine conformance

An engine implementation is conformant when it:

1. Parses and enforces envelopes per §3 (strict field set, no extras, body is a JSON object).
2. Stamps `from` and `ts` per §3 and never forwards invalid envelopes.
3. Validates and acts on every system message kind defined in §5, including the ready timeout from §2.
4. Brokers event messages via broadcast and enforces backpressure per §6.
5. Emits `error` with the correct code (§8) and closes connections when the code requires.

### Plugin conformance

A plugin implementation is conformant when it:

1. Produces envelopes per §3 (only `type` and `body` on outgoing; envelope has no other fields).
2. Sends a well-formed `ready` as its first message per §5.1, within the engine's ready timeout.
3. Sends only well-formed system messages when using `type: "system"` (kinds restricted to the ones §5 allows plugins to send, currently only `ready`).
4. Closes its connection before the `shutdown` grace window expires (the engine force-closes after).

### Canonical encoding

A conformance test suite will be published in `protocol/v0.1/conformance/` as a set of recorded message traces and expected responses. The suite pins a canonical JSON encoding (no insignificant whitespace, stable key ordering) to eliminate wire-level ambiguity across implementations. Any implementation that passes the suite is considered conformant to v0.1.
