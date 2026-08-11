# Chat extensions

A chat extension adds slash commands and small presentation hooks without forking the starter's reducer or view. Keep [`examples/nefor-agent/chat/init.lua`](../chat/init.lua) as the chat composition and set `config.active.chat_extension` to either a module name or an extension table.

```lua
-- config/init.lua
M.active.chat_extension = "my-chat-extension"
```

A complete extension module:

```lua
return {
  initial_state = function(state)
    return { custom_value = "ready" }
  end,

  commands = {
    {
      name = "echo",
      hint = "set extension state",
      takes_args = true,
      arg_completions = {
        { name = "hello", hint = "example value" },
      },
      run = function(args, state, api)
        return api.finish({ custom_value = args or "" }), {
          {
            kind = "send_to",
            target = "engine",
            body = { kind = "custom.echo", value = args },
          },
        }
      end,
    },
  },

  status_segments = function(state)
    return {
      { spans = { { text = "EXT:" .. tostring(state.custom_value) } } },
    }
  end,

  input_border_style = function(state, focused, canonical_style)
    return canonical_style
  end,
}
```

## State contract

Callbacks receive a recursively read-only view of canonical state. Mutation raises an error. Return a replacement through the command API (`api.patch`, `api.finish`, or `api.new_session`) or return a top-level patch from `initial_state`.

The helpers perform shallow merges. Keep extension state under distinct top-level keys and replace nested extension-owned values deliberately; returning a canonical nested key can replace the whole canonical table.

`initial_state(state)` may return a table patch or `nil`. Avoid canonical keys because the patch is applied to the same top-level state.

## Commands

Each command requires a non-empty `name` and `run(args, state, api)` function. Completion metadata can include `aliases`, `hint`, `takes_args`, and `arg_completions`.

A handler that returns `nil` defers to canonical command dispatch. A new command returns `(next_state, effects)`; omitted effects become an empty list.

Canonical names cannot be replaced. To add argument completions to one, declare the same name with `extend = true`:

```lua
{
  name = "model",
  extend = true,
  run = function() return nil end,
  arg_completions = {
    { name = "local/model", hint = "local profile" },
  },
}
```

Only argument completions merge. The canonical label, aliases, and handler stay authoritative.

## Presentation hooks

- `status_segments(state)` returns additional statusline segment descriptions or `nil`.
- `input_border_style(state, focused, canonical)` returns a style; `nil` preserves the canonical style.

Extensions do not own provider events, conversation projection, queue reconciliation, sessions, or the canonical reducer. If the desired behavior changes those responsibilities, it is a distribution-level chat replacement rather than an extension.

Canonical implementation: [`lua/libs/chat/extensions.lua`](../../../lua/libs/chat/extensions.lua). Executable extension and shadowing coverage lives in `plugins/nefor-tui/tests/chat_test.rs`.
