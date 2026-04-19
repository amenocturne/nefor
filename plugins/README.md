# plugins/

Lua plugins live here. First one coming: `mock-plugin/` — spawns `claude` and streams its output into nefor's event bus.

Plugin structure (conventional, not enforced):
- `<plugin-name>/init.lua` — entry point.
- `<plugin-name>/lua/` — additional modules.
- `<plugin-name>/README.md` — purpose and API.
