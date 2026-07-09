# Glossary

Quick lookup for current nefor terminology. Protocol behavior is described in [`docs/protocol.md`](protocol.md); plugin conventions are described in [`plugin-authoring.md`](plugin-authoring.md).

| Term | Definition | Defined in |
| --- | --- | --- |
| **Body** | The content object inside a plugin-authored line or delivered envelope. System bodies are handled by Lua NCP; event bodies are plugin-authored. | `docs/protocol.md` |
| **Broker** | The engine subsystem that attaches plugin transports, records published emissions in an in-memory log, and invokes Lua dispatch. It is a raw line/string bus, not the owner of NCP envelope semantics. | `engine/README.md`, `docs/protocol.md` |
| **Bus** | The Lua-mediated stream of entries explicitly published with `nefor.engine.send`. Default routing offers Step events to ready wrappers, skips self-emissions, respects targets/legacy peer prefixes, and lets wrappers drop or transform. | `docs/protocol.md` |
| **Engine** | The `nefor` binary: process spawner, line router, Lua host, and in-memory dispatch log owner. It is session-blind. | `engine/README.md` |
| **Envelope** | Conventional JSON object used on the bus. Plugin-authored outgoing lines commonly include `type` and `body`; delivered bus envelopes include `type`, `from`, `ts`, and `body`. Current stamping/routing semantics are Lua-owned. | `docs/protocol.md` |
| **Event** | A message with `type: event`; its body is plugin-authored and opaque to the Rust engine. | `docs/protocol.md` |
| **Goodbye event** | Convention: a plugin-namespaced event a plugin may publish before exiting. It is ordinary event traffic, not engine-synthesized shutdown. | `plugin-authoring.md` |
| **Hello event** | Convention: a plugin-namespaced event a plugin emits after `ready_ok` to advertise version/capabilities. | `plugin-authoring.md` |
| **Kind** | The discriminator inside `body` identifying a sub-shape. | `docs/protocol.md` |
| **Manifest** | Convention: a plugin-namespaced event declaring what kinds the plugin consumes/emits and what version it advertises. | `plugin-authoring.md` |
| **NCP** | Nefor Composition Protocol as currently implemented by Lua over the engine string bus. | `docs/protocol.md` |
| **Plugin** | Any subprocess connected to the engine over stdin/stdout and participating in the current line protocol. | `docs/protocol.md` |
| **Ready** | System message a plugin sends with `protocol_version = 0.1` to enter the ready set. | `docs/protocol.md` |
| **Ready OK** | Lua NCP's direct reply accepting `ready`. | `docs/protocol.md` |
| **Runner** | The engine subsystem that resolves plugin binaries, starts subprocesses with direct `Command::new`, and bridges stdio. | `docs/principles.md` |
| **System message** | A message with `type: system`; current shipped Lua handles `ready` from plugins plus direct `ready_ok`/`error` replies to plugins. | `docs/protocol.md` |
