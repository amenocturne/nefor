# Approval Model

The starter approval model has two axes:

1. Mode: how much autonomy the user granted.
2. Action class: why an action needs approval.

Keeping those axes separate prevents plan review, ordinary tool risk, and hard
danger from collapsing into one generic "permission denied" path.

## Modes

| Mode | Contract |
| --- | --- |
| `safe` | The human is in the loop. Safe actions run; anything requiring judgment or risk acceptance asks. |
| `auto` | The agent runs autonomously. Safe actions run; safe human-judgment steps are auto-resolved; risky actions that need a human are denied. |
| `yolo` | The agent is allowed to do whatever the user asked for. All gates approve. |

## Action Classes

| Class | Meaning | Examples |
| --- | --- | --- |
| `safe` | Non-destructive mechanical action. | `read_file`, `list_dir`, read-only graph dispatch. |
| `human` | Safe action whose value is the human judgment itself. | `write-review` plan approval. |
| `guarded` | Operation that can be acceptable, but needs explicit risk acceptance when no autonomous policy proves it safe. | `bash` that `da` cannot prove safe. |
| `forbidden` | Operation classified as dangerous. | `bash` that `da` rejects. |

## Decision Table

| Action class | `safe` mode | `auto` mode | `yolo` mode |
| --- | --- | --- | --- |
| `safe` | approve | approve | approve |
| `human` | ask/block | auto-resolve | approve |
| `guarded` | ask | deny | approve |
| `forbidden` | ask | deny | approve |

## Current Layering

- `tool-gate` owns the generic prompt/forward/deny transport.
- `tool-validator` owns tool-risk classification before a popup reaches chat.
- `lead-workflow` owns plan approval and writer graph dispatch policy.

Plan approval is not a dangerous action. In `safe`, it blocks for the user's
verdict. In `auto`, it records the plan and auto-resolves so the agent can keep
running. Dangerous tool calls inside a dispatched graph are still classified by
`tool-validator`.
