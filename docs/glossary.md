# Glossary

Quick lookup for nefor and NCP terminology. All of these are defined in context in the [NCP spec](../protocol/v0.1/spec.md); this page is purely a convenience index.

| Term               | Definition                                                                                                              | Defined in                                   |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| **Attach**         | The handshake a plugin performs to join the bus.                                                                        | NCP §5.1                                     |
| **Body**           | The content field of an envelope. Spec-defined shapes for system messages, plugin-authored for events.                  | NCP §3                                       |
| **Bus**            | The engine's broadcast mechanism. Every event message reaches every attached plugin.                                    | NCP §6                                       |
| **Engine**         | A process implementing NCP's engine role: runs plugins, brokers messages. Reference implementation: the `nefor` binary. | NCP §1                                       |
| **Envelope**       | The top-level JSON object with `type`, `from`, `ts`, `body`.                                                            | NCP §3                                       |
| **Event**          | A message with `type: "event"`. Body is plugin-authored and opaque to the engine.                                       | NCP §4                                       |
| **Kind**           | The discriminator inside `body` identifying the sub-shape.                                                              | NCP §5                                       |
| **NCP**            | Nefor Composition Protocol. The communication protocol between the engine and its plugins.                              | [`protocol/v0.1/`](../protocol/v0.1/spec.md) |
| **Plugin**         | Any process that speaks NCP and has attached to an engine.                                                              | NCP §1                                       |
| **System message** | A message with `type: "system"`. Body follows a shape defined in NCP §5.                                                | NCP §4                                       |
