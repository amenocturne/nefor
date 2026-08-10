# Approval architecture

The shipped starter has three related but independent approval systems:

1. **Ordinary tool permission** — `tool-gate` transports requests and
   `tool-validator` applies capability, mode, and risk policy.
2. **Write-review plan approval** — `lead-workflow` owns transient plan state
   and the decision that authorizes a later `write_file`.
3. **MAG human nodes** — typed graph nodes suspend a run for a correlated
   `mag.ApprovalReply`.

The [user permissions guide](user/permissions.md) is the authority for current
starter behavior. This page documents implementation ownership and invariants;
one system's decision must not be treated as proof for another.

## Ordinary tool invariants

Mode never invents a capability. The validator first checks structural
authority:

1. malformed capability data or an allowlist excluding the requested tool is
   denied in every mode;
2. a read-only principal cannot edit or write;
3. only then does mode and risk policy apply.

A missing allowlist retains legacy unrestricted behavior. New callers should
always send a complete allowlist.

After those checks, ordinary tool classes behave as follows:

| Action class                   | `safe`  | `auto`  | `yolo`  |
| ------------------------------ | ------- | ------- | ------- |
| proven safe                    | approve | approve | approve |
| needs human judgment           | ask     | deny    | approve |
| guarded by risk classification | ask     | deny    | approve |
| classified dangerous           | ask     | deny    | approve |

`auto` is intentionally not “safe with automatic clicks.” It denies ordinary
requests that need human risk acceptance. “Classified dangerous” is a risk
classification rather than an absolute runtime prohibition: `safe` lets a
human accept it, and `yolo` has already accepted it.

Important starter cases:

- configured auto tools pass after capability validation;
- `edit_file` passes only for write-capable agents;
- `write_file` requires a live approved plan in `safe` and `auto`; `yolo`
  bypasses that plan check;
- non-read-only `bash` is checked by configured fast paths and then `da`;
- missing `da` is an installation error, not an approval fallback;
- a generic tool whose complete allowlist is read-only passes automatically.

See [Providers and tools](customization/providers-and-tools.md) for the
validator registration seam.

## Write-review ownership and lifetime

`lead-workflow` owns write-plan state and writer dispatch. The `write-review`
submission tool itself is automatic in the starter; its plan verdict is not an
ordinary tool decision.

In `safe`, the workflow parks a plan for `/approve` or `/reject`. In `auto` and
`yolo`, starter policy approves it immediately so autonomous writer execution
can proceed.

Approval is transient authority:

- it is turn-scoped and expires after later user input;
- session replay and resume do not reconstruct it;
- cancellation or killed runs invalidate parked correlations.

A resumed session must obtain fresh authority before a new write that requires
a plan.

## MAG human-node ownership

MAG's human factory is a typed graph boundary. It emits a
`mag.approval_request` for a `mag.ApprovalRequest`; a reply returns through
`mag.apply` as `mag.ApprovalReply` and produces typed `human.Approved` or
`human.Rejected`. Cancellation emits `mag.approval_cancel`.

This protocol belongs to the MAG plugin and run context. The current starter's
chat actor presents every live request and does not consult safe/auto/yolo when
forming the verdict. Factory registration, correlation tables, and approval
internals are kernel APIs, not external customization seams. Author the node
through the public MAG language and contracts; see
[`plugins/mag/docs/actor-model.md`](../plugins/mag/docs/actor-model.md) for the
boundary.

## Evidence

- Ordinary policy: [`lua/libs/tool-validator/init.lua`](../lua/libs/tool-validator/init.lua)
  and `tests/lua/tool-validator/mode_test.lua`.
- Write-review: `lua/libs/lead-workflow/init.lua` and
  `tests/lua/lead-workflow/workflow_test.lua`.
- MAG human nodes: `plugins/mag/lua/mag-kernel/factories/human.lua`,
  `starter/chat/update.lua`, and `tests/lua/mag-kernel/human_test.lua`.
