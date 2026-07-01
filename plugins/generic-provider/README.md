# generic-provider

NCP v0.1 plugin: type-registry hub for the canonical provider protocol.

Owns five canonical types that every provider-shaped reasoner agrees on:

- `generic-provider.ProviderIn` -- standard chat-completion request
- `generic-provider.ProviderOut` -- standard chat-completion response
- `generic-provider.ChatHistory` -- provider-shaped reasoner state
- `generic-provider.NoState` -- unit/empty state for stateless reasoners
- `generic-provider.FinalAnswer` -- escape-edge type emitted by `tool_split`

On startup, registers these types via MAG's compile-time routing. Concrete
providers (openai-provider, anthropic-provider, ...) separately declare
`Into`/`From` conversions against these tags. This plugin does not run
models or hold sessions -- it is a passive hub that makes the canonical
type tags exist on the wire.
