# Model Routing

Role-based model routing assigns different LLMs to different agent roles. Configured in flavour config files (`nefor/config/test.yaml` and `nefor/config/prod.yaml`), loaded by `nefor/config/index.ts`, and consumed by `disguise.ts` when defining agent configs.

## Role Definitions

| Role | Used By | Optimized For |
|------|---------|---------------|
| `orchestrator` | Lead disguise (main session) | Instruction following, planning, coordination |
| `worker` | Builder subagent | Raw coding power, implementation |
| `reviewer` | Reviewer subagent | Evaluation, critique, synthesis |
| `explorer` | Explorer subagent | Codebase navigation, summarization |
| `tester` | Tester subagent | Test execution, failure diagnosis |

## Configuration

Flavour config files in `agents/pi/nefor/config/`:

```yaml
# config/test.yaml (OpenRouter models for development)
provider: openrouter
models:
  orchestrator: openrouter/anthropic/claude-sonnet-4
  worker: openrouter/anthropic/claude-sonnet-4
  reviewer: openrouter/anthropic/claude-sonnet-4
  explorer: openrouter/anthropic/claude-sonnet-4
  tester: openrouter/anthropic/claude-sonnet-4

# config/prod.yaml (Nestor/Tinkoff internal models)
provider: nestor
models:
  orchestrator: tgpt/qwen35-397b-a17b-fp8
  worker: tgpt/qwen35-397b-a17b-fp8
  reviewer: tgpt/qwen35-397b-a17b-fp8
  explorer: tgpt/qwen35-397b-a17b-fp8
  tester: tgpt/qwen35-397b-a17b-fp8
```

The active config is selected via `config` field in `.pi/agentic-kit.json` (written by the installer). If not set, defaults to `test`.

### Config Loading (`nefor/config/index.ts`)

```typescript
import config from "./config/index.ts";

// config.provider → "openrouter" or "nestor"
// config.models.orchestrator → model ID string
// config.models.worker → model ID string
// etc.
```

`loadConfig()` reads the config name from `.pi/agentic-kit.json`, loads the corresponding YAML file, and exports the parsed `FlavourConfig` as the default export.

## How Routing Works

There is no separate model-router extension or `getModelForRole()` function. Model routing happens directly in `disguise.ts` via the imported config:

```typescript
import config from "./config/index.ts";

const explorer: AgentConfig = {
  model: config.models.explorer,
  prompt: "prompts/explorer.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

const builder: AgentConfig = {
  model: config.models.worker,
  prompt: "prompts/builder.md",
  tools: { allow: ["read", "write", "edit", "bash", "grep", "find", "ls", "glob"] },
};
```

Each agent config references the appropriate role from the config. When the disguise extension spawns a subagent via `ctx.spawn(builder, task)`, the framework reads `builder.model` and passes it to the Pi subprocess.

## Current Model Landscape

Available on the internal Nestor/Tinkoff GPU cluster:

| Model | Active Params | Strengths | Weaknesses | Typical Role |
|-------|--------------|-----------|------------|--------------|
| `tgpt/qwen35-397b-a17b-fp8` | 17B (MoE) | Raw coding/reasoning, 1M context | Poor instruction following (IFEval 82), overthinking loops | worker |
| `tgpt/gpt-oss-120b` | 120B | More predictable, better constraint compliance | Lower ceiling on all benchmarks | -- |
| `tgpt/qwen3-next-80b-a3b-instruct` | 3.9B (MoE) | Best IFEval (87.6), fastest inference (~346 tok/s), good tool calling (BFCL 70.3) | Weaker deep reasoning (GPQA 72.9) | orchestrator, reviewer |

**Routing rationale**: The orchestrator needs reliable instruction following (don't implement, delegate, follow the workflow). Qwen3-next-80b is best at this despite weaker reasoning. Workers need raw coding power for bounded tasks where instruction following matters less. Qwen 3.5 397B excels here.

## The Nestor Provider

The `nestor-provider` extension connects Pi to Tinkoff's internal LLM API. The API is OpenAI-compatible, so Pi's built-in OpenAI completions streaming works with a custom `Nestor-Token` header.

### Auth Flow

```
dp auth login (user runs in terminal)
      |
      v
dp auth print-token -> DP access token
      |
      v
POST /api/v2/token (exchange for JWT)
      |
      v
Nestor JWT (used in all API calls as Nestor-Token header)
```

The `dp` binary manages the DevPlatform auth session. It stores tokens in `~/.nessy/dp_v13.4.2/` or its own default workdir. The extension finds it at `/usr/local/bin/dp` or `~/.nessy/dp_v13.4.2/dp`.

On session start, the extension silently tries the existing DP session. If it works, no `/login` is needed. If not, the user runs `/login nestor` which prompts them to run `dp auth login` in another terminal.

### Model Discovery

After auth, the extension fetches models from `GET /api/v1/cli/models`. The response doesn't include context window or max token limits, so these are inferred from model name patterns:

- `qwen35` -> 1M context, 16K max tokens
- `qwen3` -> 128K context, 8K max tokens
- `gpt-oss` -> 128K context, 4K max tokens

### API Compatibility

The Nestor API is OpenAI-compatible with these caveats:

- `parallel_tool_calls: false` is set on every request with tools. Part of the OpenAI chat completions spec, so it should work even though other params are ignored.
- `supportsDeveloperRole: false` -- the API doesn't support the `developer` message role
- Thinking output comes as `<think>...</think>` tags in content, not as `reasoning_content`. The extension has a streaming interceptor that parses these tags into proper Pi thinking events.

## parallel_tool_calls: false

Set by nestor-provider on every completion request that includes tools:

```typescript
onPayload: (payload) => {
  if (p.tools && p.tools.length > 0) {
    p.parallel_tool_calls = false;
  }
  return p;
},
```

This is schema-level enforcement of the "one tool per message" rule. It tells the API to constrain the model to return at most one tool call per response.

Whether the Nestor API actually respects this parameter is uncertain -- it silently ignores many OpenAI parameters. The behavioral reminder `multi_tool_attempt` serves as a fallback detector (see [behavioral-reminders.md](behavioral-reminders.md)).

## Sampling Parameters

**The Nestor API silently ignores all sampling parameters.** Tested 2026-04-01: `temperature=2.0` produces identical output to `temperature=0.0`. Bogus parameters also pass silently -- the API is fully permissive and drops unknown fields.

This means recommended Qwen sampling params are not available:
- `presence_penalty=1.5` (targets overthinking loops) -- **not available**
- `temperature=0.6, top_p=0.95, top_k=20` (recommended for coding) -- **not available**

Compensated by:
- Behavioral reminders (`self_contradiction` detects overthinking loops)
- Prompt-level directives ("pick an approach and commit to it")
- The one-tool-per-message rule (reduces opportunity for spiraling)

If the Nestor AI team adds sampling param support later, add them to the `onPayload` callback in `nestor-provider/index.ts`.
