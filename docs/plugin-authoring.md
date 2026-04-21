# Plugin authoring guide

This is a guide for writing nefor plugins. It documents ecosystem conventions that are not part of NCP (the protocol spec lives at [`protocol/v0.1/spec.md`](../protocol/v0.1/spec.md)). Following them is optional but helps your plugin interoperate with the rest of the ecosystem.

The spec tells you what you MUST do. This document tells you what you're well advised to do.

## Naming your plugin

Plugin names SHOULD be lowercase alphanumeric with hyphens — e.g., `my-plugin`, `fast-bus`, `some-harness`. Dots are reserved for the `kind` prefix convention below, so avoid them in plugin names.

Names are globally unique within a running engine (enforced via the `name_taken` error). Pick something identifiable and unlikely to collide with common words.

## Kind namespacing

Event-message `kind` values SHOULD be prefixed with your plugin's name and a dot:

```
plugin-a.event_occurred
plugin-a.run_action
plugin-b.input_received
plugin-b.render_complete
plugin-c.state_changed
```

Why: a message's `kind` is global across the bus. If your plugin emits a `kind` without its name as a prefix, another plugin's message could collide with yours. The prefix convention makes kinds globally unique by piggybacking on already-unique plugin names.

System `kind` values defined by NCP are unprefixed — they are owned by the protocol spec, not by any plugin.

## Request/response pattern

NCP does not define a request/response primitive. When your plugin needs the pattern, implement it in `body`. The ecosystem convention looks like this:

```json
// Request (plugin-a asks plugin-b to run something):
{ "type": "event", "from": "plugin-a", "ts": "…",
  "body": { "kind": "plugin-b.run_action",
            "request_id": "plugin-a:42",
            "args": { … } }}

// Response (plugin-b answers):
{ "type": "event", "from": "plugin-b", "ts": "…",
  "body": { "kind": "plugin-b.run_action_ok",
            "in_reply_to": "plugin-a:42",
            "result": { … } }}
```

Conventions:

- `request_id` SHOULD be unique within the sender. A common scheme: `<plugin-name>:<counter>`.
- `in_reply_to` SHOULD echo the request_id exactly.

The requesting plugin filters incoming events by `from`, `kind`, and `in_reply_to` to match responses to requests.

## Addressed messages

To hint that a message is directed at a specific recipient, include an advisory `to` field in body:

```json
{ "body": { "kind": "…", "to": "plugin-b", … } }
```

This is a hint, not a filter. Every plugin still receives the message (the bus is broadcast); other plugins can fast-skip if `body.to` is present and not equal to their own name. Useful for request/response flows where the receiver-role is unambiguous.

## More to come

This guide will grow as ecosystem conventions stabilise. If you find yourself implementing a pattern that feels universal — a way to report progress, a way to discover peer capabilities, a way to offer services — propose it here.
