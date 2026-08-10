# Approval model

The shipped starter has two related approval layers:

1. **Tool and plan policy** — the starter's mode, capability, risk classification, and user prompt behavior.
2. **MAG human nodes** — typed graph nodes that suspend a run for an `ApprovalReply`.

They are separate systems. A MAG human node is not the tool validator, and a tool popup is not automatically a MAG graph edge.

## Invariants before mode

Mode never invents a capability. The validator first checks structural authority:

1. malformed capability data or an allowlist excluding the requested tool is denied in every mode;
2. a read-only principal cannot edit or write;
3. only then does mode/risk policy apply.

A missing allowlist retains legacy unrestricted behavior. New callers should always send a complete allowlist.

## Modes

| Mode   | Contract                                                                                                                                        |
| ------ | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `safe` | A human is available. Safe actions run and interaction-requiring actions may prompt.                                                            |
| `auto` | No human is available for tool popups. Interaction-requiring tool requests are denied; lead write-review is recorded and approved autonomously. |
| `yolo` | Broad risk is accepted after capability and structural validation.                                                                              |

`auto` is intentionally not “safe with automatic clicks.” It denies tool requests that need human risk acceptance. The lead workflow currently treats plan review differently: both `auto` and `yolo` approve submitted write plans immediately so autonomous writer execution can proceed.

## Tool action classes

| Class               | Meaning                                                                  | Examples                              |
| ------------------- | ------------------------------------------------------------------------ | ------------------------------------- |
| `safe`              | Non-destructive action proven safe by capability/policy.                 | canonical read-only tools             |
| `human`             | The value of the step is human judgment.                                 | write-review in `safe`                |
| `guarded`           | May be acceptable but needs risk acceptance without an autonomous proof. | non-read-only `bash` that `da` defers |
| `classifier-denied` | `da` classifies the command as dangerous.                                | destructive shell command             |

After invariants, the mode table is:

| Action class      | `safe`    | `auto`  | `yolo`  |
| ----------------- | --------- | ------- | ------- |
| safe              | approve   | approve | approve |
| human tool step   | ask/block | deny    | approve |
| guarded           | ask       | deny    | approve |
| classifier-denied | ask       | deny    | approve |

“Classifier-denied” is a risk classification, not an absolute runtime prohibition: `safe` lets the human accept the risk and `yolo` has already accepted it.

## Current tool layering

- `tool-gate` owns advertisement, correlation, prompt/forward/deny transport.
- `tool-validator` validates capability and classifies tool risk before chat sees a popup.
- `lead-workflow` owns write-plan state and writer dispatch.

Important special cases:

- configured auto-approved tools pass after capability validation;
- `edit_file` passes only for write-capable agents;
- `write_file` requires a live approved plan in `safe` and `auto`; `yolo` bypasses that plan check;
- non-read-only `bash` is checked by configured fast paths and then `da`;
- missing `da` is an installation error, not an approval fallback;
- a generic tool whose complete allowlist is read-only passes automatically.

See [Providers and tools](customization/providers-and-tools.md) for the validator registration seam.

## Plan approval lifetime

In `safe`, `write-review` parks a plan for `/approve` or `/reject`. In `auto` and `yolo`, it is approved immediately.

Approval is transient, not durable authority:

- it is turn-scoped/single-use and expires after later user input;
- session replay/resume does not reconstruct it;
- cancellation or killed runs invalidate parked correlations.

A resumed session must obtain fresh authority before a new write that requires a plan.

## MAG human approval

MAG's human factory is a typed graph boundary. It emits a `mag.approval_request` for a `mag.ApprovalRequest`; a reply returns through `mag.apply` as `mag.ApprovalReply` and produces typed `human.Approved` or `human.Rejected`. Cancellation emits `mag.approval_cancel`.

This protocol belongs to the MAG plugin/run context. Factory registration, correlation tables, and approval internals are kernel APIs, not external customization seams. Author the human node through the public MAG language and contracts; see `plugins/mag/docs/actor-model.md` for its approval boundary and `tests/lua/mag-kernel/human_test.lua` for executable behavior.

## Evidence

Tool policy is implemented by [`lua/libs/tool-validator/init.lua`](../lua/libs/tool-validator/init.lua) and tested in `tests/lua/tool-validator/mode_test.lua`. Plan behavior is implemented by `lua/libs/lead-workflow/init.lua` and tested in `tests/lua/lead-workflow/workflow_test.lua`.
