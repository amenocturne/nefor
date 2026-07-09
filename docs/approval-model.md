# Approval Model

The starter approval model has two axes:

1. Mode: how much autonomy the user granted.
2. Action class: why an action needs approval.

Keeping those axes separate prevents plan review, ordinary tool risk, and hard danger from collapsing into one generic permission path.

## Modes

| Mode | Contract |
| --- | --- |
| `safe` | Human is in the loop. Safe actions run; actions requiring judgment or risk acceptance ask. |
| `auto` | Autonomous but not allowed to invent human approval. Safe actions run; risky actions and write-review human steps are denied. |
| `yolo` | User has accepted broad risk. Gates approve. |

## Action Classes

| Class | Meaning | Examples |
| --- | --- | --- |
| `safe` | Non-destructive mechanical action. | `read_file`, read-only graph dispatch, read-only tools. |
| `human` | Action whose value is the human judgment itself. | `write-review` plan approval. |
| `guarded` | Operation that can be acceptable, but needs explicit risk acceptance when no autonomous policy proves it safe. | `bash` that `da` cannot prove safe. |
| `forbidden` | Operation classified as dangerous. | `bash` that `da` rejects. |

## Decision Table

| Action class | `safe` mode | `auto` mode | `yolo` mode |
| --- | --- | --- | --- |
| `safe` | approve | approve | approve |
| `human` | ask/block | deny | approve |
| `guarded` | ask | deny | approve |
| `forbidden` | ask | deny | approve |

## Current Layering

- `tool-gate` owns the generic prompt/forward/deny transport.
- `tool-validator` owns tool-risk classification before a popup reaches chat.
- `lead-workflow` owns plan approval and writer graph dispatch policy.

Current shipped behavior:

- `write-review` blocks for `/approve` or `/reject` in `safe`.
- `write-review` is denied in `auto`; the agent cannot auto-resolve a human review.
- `write-review` is auto-approved in `yolo`.
- Read-only tools and configured auto-approved tools can pass without a popup.
- `edit_file` is approved for non-read-only agents by tool-validator policy.
- `write_file` requires an approved plan.
- `bash` is checked by `da` plus configured fast paths; missing `da` is an install/configuration error, not an approval fallback.
