# nefor

The engine binary. Process spawner, line router, and Lua host.

Startup: parse CLI args, boot the Lua VM with a broker routing sink, run `init.lua`, then branch into serve mode (spawn plugins, run the broker) or plugin-dispatch mode (invoke a virtual plugin's CLI function).

The engine is session-blind: it owns no session ID, writes no persistent session log, and does not parse envelope bodies. The selected Lua composition chooses the session root and wires persistence, replacement, resume, and replay through `lua/libs/sessions` (`libs.sessions`). The starter therefore preserves session behavior without giving Rust a session concept.
