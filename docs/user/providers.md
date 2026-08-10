# Providers

> **Unreleased** — documents `505a764`, after `v0.4.0`.

Nefor's [starter composition](../../starter/config/init.lua) is usable without credentials or a running model server: it starts the deterministic mock provider by default. Real providers are opt-in. The same composition can run several providers at once, and the chat surface discovers their models through `/model`.

## Provider overview

| Provider          | Enable it                              | Authentication                                   | Models                    | Images                                     | Native compaction | Account usage |
| ----------------- | -------------------------------------- | ------------------------------------------------ | ------------------------- | ------------------------------------------ | ----------------- | ------------- |
| Mock              | Enabled by default                     | None                                             | `mock-model`              | No                                         | No                | No            |
| ChatGPT           | `NEFOR_ENABLE_CHATGPT=1`               | ChatGPT subscription OAuth                       | Fetched from ChatGPT      | Yes, when the selected model supports them | Yes               | Yes           |
| Ollama            | `NEFOR_ENABLE_OLLAMA=1`                | None; starter supplies a local placeholder token | Fetched from `/v1/models` | No                                         | No                | No            |
| OpenAI-compatible | Add an `openai` provider to the config | `OPENAI_PROVIDER_API_KEY`                        | Fetched from `/v1/models` | No                                         | No                | No            |

“Images” here means image media returned by a tool such as `read_image`. It does not mean that every listed model is vision-capable.

## Default: the mock provider

The starter always includes this provider:

```lua
{
  kind        = "mock",
  name        = "mock-plugin",
  mock_script = "mock-provider/init.lua",
}
```

It is the default provider and model unless overridden:

```sh
NEFOR_DEFAULT_PROVIDER=mock-plugin \
NEFOR_DEFAULT_MODEL=mock-model \
nefor
```

The mock is a scripted test machine, not an LLM. It gives deterministic responses for the starter's demonstrations and tests, advertises only `mock-model`, and needs no network or credentials. Its behavior is defined in [`starter/mock-provider/init.lua`](../../starter/mock-provider/init.lua).

## ChatGPT subscription provider

The `chatgpt-provider` uses OpenAI's ChatGPT Responses backend with ChatGPT-subscription OAuth credentials. It is separate from an OpenAI API-key account.

Enable it when starting Nefor:

```sh
NEFOR_ENABLE_CHATGPT=1 nefor
```

The starter adds a provider named `chatgpt`; it does not make it the default. Open `/model` and select one of its models, or set defaults after identifying a model from the picker:

```sh
NEFOR_ENABLE_CHATGPT=1 \
NEFOR_DEFAULT_PROVIDER=chatgpt \
NEFOR_DEFAULT_MODEL=<model-id> \
nefor
```

Do not guess or permanently document a ChatGPT model ID. The plugin fetches the current catalog from the backend after authentication and has no `--model` option.

### Log in and log out

With ChatGPT enabled, use the chat commands:

```text
/login chatgpt
/logout chatgpt
```

Omitting the provider opens a picker. Login launches the OAuth PKCE flow; logout clears the active OAuth credentials, removes the local auth file, and attempts to revoke the refresh token. The plugin refreshes expiring credentials while running.

For login before launching the TUI, the standalone command is also supported:

```sh
chatgpt-provider login
```

OAuth credentials are stored under the resolved Nefor data directory as `chatgpt-auth.json` (normally `$XDG_DATA_HOME/nefor/chatgpt-auth.json`, or `$NEFOR_DATA_DIR/chatgpt-auth.json` when set). On Unix, newly written credential files use mode `0600`. Do not copy tokens into `init.lua` or command-line arguments.

### Models, reasoning, usage, and compaction

- `/model` fetches and displays the authenticated account's current model inventory. Selecting a row changes both provider and model.
- `/think low|medium|high|xhigh` (also `/effort`) changes reasoning effort for the active provider. A backend may reject an unsupported level; model-catalog capabilities and defaults are used when available.
- `/usage` displays ChatGPT's primary and secondary quota windows, reset times, plan information, and credits. ChatGPT usage is also refreshed periodically and after successful responses; other bundled providers do not expose account quota.
- `/compact` requests native Responses compaction. Nefor persists the provider-owned opaque checkpoint and can restore it for later turns on the same provider/model path. The complete transcript remains the fallback if the checkpoint cannot be reused.

ChatGPT converts image tool results into Responses image inputs only when the selected model supports images. Otherwise it reports an explicit capability error rather than silently dropping the image.

## Ollama

Run an Ollama server, pull at least one model, then enable the starter integration:

```sh
ollama pull qwen3
NEFOR_ENABLE_OLLAMA=1 nefor
```

The starter config is equivalent to:

```lua
{
  kind         = "openai",
  name         = "ollama",
  static_token = "ollama-local",
  base_url     = "http://localhost:11434",
  extra_args   = {},
}
```

`openai-provider` appends `/v1/chat/completions` for completions and queries `/v1/models` for `/model`. The placeholder token is injected over Nefor's internal bus; it is not a real secret and Ollama does not validate it by default.

You may pin a startup model in a copied config by adding `model = "qwen3"`, but leaving it unset lets `/model` reflect the server's installed inventory. Model IDs must match Ollama exactly, including tags when applicable.

Ollama uses the generic OpenAI-compatible path. It can stream reasoning text from backends that emit `delta.reasoning`, and `reasoning_effort` is sent when selected, but support is model/server-specific. This provider is text-only in Nefor: image tool output becomes an explicit unsupported-image message. Native `/compact` is also unsupported.

## Other OpenAI-compatible providers

Add another entry to `providers` in a copied [`starter/config/init.lua`](../../starter/config/init.lua). For a standard bearer-token service:

```lua
{
  kind       = "openai",
  name       = "openrouter",
  base_url   = "https://openrouter.ai/api",
  model      = "<provider-model-id>", -- optional
  extra_args = {},
}
```

Then source the key into the environment before launching Nefor:

```sh
export OPENAI_PROVIDER_API_KEY='…'
nefor --config /path/to/your/config
```

The provider constructs these endpoints from `base_url`:

```text
<base_url>/v1/chat/completions
<base_url>/v1/models
```

Therefore `base_url` must be the service root _before_ `/v1`, unless the service intentionally nests its OpenAI API there (as in the OpenRouter example).

For a backend that expects a raw token in a custom header rather than `Authorization: Bearer …`, pass only the non-secret header name through config:

```lua
{
  kind       = "openai",
  name       = "internal",
  base_url   = "https://llm.example.test",
  extra_args = { "--auth-header", "Nestor-Token" },
}
```

Keep the token in `OPENAI_PROVIDER_API_KEY`. Do **not** put `--api-key`, a real `static_token`, or a literal token in `init.lua`: config is ordinary source, while CLI arguments may be visible in process listings. The environment fallback is the secure supported route for this provider.

The generic provider expects OpenAI-compatible streaming chat completions and model-list responses. “OpenAI-compatible” implementations vary: tool calling, reasoning fields, usage frames, context-window metadata, and model metadata may be missing or shaped differently. Validate the intended backend with its actual models.

## Selecting models and reasoning

Use `/model` with no argument for the canonical multi-provider picker. It requests each provider's inventory and shows connection state. Selecting a model sets the provider and model together.

`/model <model-id>` is less precise when several providers are connected: the current chat implementation targets the alphabetically first connected provider rather than accepting a provider-qualified model. Prefer the picker in multi-provider configurations.

The selected provider, model, and reasoning effort appear in the status line. `/think` accepts `low`, `medium`, `high`, or `xhigh`; these are forwarded to the backend and are not guaranteed for every model. The starter's orchestration profiles also carry efforts (`fast`, `standard`, `deep`, and `max`), while explicit model capability metadata may provide a model default.

Environment defaults configure the lead workflow at startup:

```sh
NEFOR_DEFAULT_PROVIDER=chatgpt \
NEFOR_DEFAULT_MODEL=<model-id> \
nefor
```

The named provider must be enabled and the model must exist. Changing `/model` affects subsequent turns; a provider switch may prevent reuse of a provider-specific compaction checkpoint.

## Multi-provider caveats

- Provider `name` values are bus namespaces and must be unique. Two entries with the same name collide.
- Every enabled provider appears in `/model`, including the always-on mock.
- `OPENAI_PROVIDER_API_KEY` is inherited by every `openai-provider` process. The starter spawn API has no per-instance environment map, so multiple hosted OpenAI-compatible providers cannot safely receive different keys through this composition. Use one environment-keyed hosted instance at a time, or provide a custom composition/auth boundary; never solve this by committing keys.
- A static `model` is only a default. `/model` still queries the backend and can switch it.
- ChatGPT owns native compaction artifacts. Switching provider or model retains the full transcript as a fallback, but does not make another provider understand that artifact.
- The generic OpenAI-compatible provider explicitly rejects native compaction, and the mock does not provide it. These paths do not perform a local summary fallback.
- Only ChatGPT currently translates image tool results to model input. Generic OpenAI-compatible providers, including Ollama, are text-only even if the remote model itself supports vision.

For the lower-level plugin flags and wire behavior, see the [`openai-provider` README](../../plugins/openai-provider/README.md) and [`chatgpt-provider` README](../../plugins/chatgpt-provider/README.md).
