# Permissions

Permission mode and approval kind are separate. The starter has **three
user-facing approval systems**:

1. ordinary tool-risk decisions;
2. write-review plan decisions;
3. explicit MAG human-approval nodes.

`/safe`, `/auto`, and `/yolo` govern the first two. MAG human nodes remain
explicitly interactive in the current starter; they do not inherit an automatic
mode verdict.

## Modes

| Mode   | Ordinary tools                                                           | Write-review                            |
| ------ | ------------------------------------------------------------------------ | --------------------------------------- |
| `safe` | Proven-safe actions run; deferred or guarded requests ask.               | Waits for a human decision.             |
| `auto` | Requests needing human risk acceptance are denied rather than prompting. | Immediately approved by starter policy. |
| `yolo` | Gated requests pass after structural capability checks. Dangerous.       | Immediately approved by starter policy. |

Use `/safe`, `/auto`, or `/yolo` to switch live. At process start,
`nefor run --mode safe|auto|yolo` selects the initial mode and `--yolo` aliases
`--mode yolo`. Repeated startup mode controls are interpreted in argument order,
so the last wins. Without an explicit startup mode, startup and resume are safe;
mode is not restored as durable authority from a prior process.

## 1. Ordinary tool permission

The `tool-gate` transports gated calls and decisions. Before chat sees a popup,
the starter `tool-validator` checks capability membership and classifies the
request. It emits exactly one approve, deny, or popup decision.

### Structural checks first

When an invocation has a tool allowlist, it must be a valid dense list
containing the requested tool. Invalid or excluding capability data is denied
even in `yolo`. A missing allowlist is the legacy unrestricted shape; a
`read_only` boolean alone is ignored. An invocation is considered read-only
only when **every** allowed tool belongs to the composition's canonical
read-only inventory.

### Current starter policy

- composition-configured auto tools, including read/context tools and MAG
  control-plane dispatch tools, pass without a popup;
- read-only-capability tools pass;
- `edit_file` passes for write-capable agents and is denied to read-only agents;
- `write_file` passes only when a current approved write-review plan exists,
  except for the earlier `yolo` gate bypass;
- write-capable `shell.script` is checked by `da` plus configured fast paths;
- missing or unusable `da` is an installation error, not permission fallback;
- any remaining request is interactive in `safe`, denied with recovery guidance
  in `auto`, and approved in `yolo`.

For an ordinary permission popup, `A` or `Enter` approves; `D` or `Esc` denies.
Multiple requests queue rather than replacing the active popup. Approval
authorizes that invocation; it is not a blanket plan approval or MAG graph
modification.

## 2. Write-review plan approval

`write-review` (alias `submit-plan`) is a lead-workflow tool. Calling it does not
itself write files: it submits one plan and renders a review block. The starter
tool gate lists this submission tool as automatic, avoiding a redundant
ordinary-tool popup. The **plan verdict** is the separate approval.

In `safe`, inline review blocks the lead tool call:

- `/approve [note]` marks the plan approved and resumes the lead;
- `/reject [reason]` marks it rejected and instructs the lead to revise;
- any other submitted text discards the plan and passes the text as feedback.

Only one plan slot exists. A newer plan supersedes an earlier pending one.
Approval is scoped to the current turn: the next genuine user message clears
decided plan authority, and session end or restart clears it. Plan state is
intentionally not rebuilt by replay. A writer graph and `write_file` therefore
need a fresh valid plan cycle after that boundary.

Both `auto` and `yolo` immediately mark write-review approved and return a
mode-specific bypass notice. That is a starter policy choice, not an engine
guarantee and not equivalent to a human verdict. Downstream configurations may
choose a stricter policy.

`view = "web"` routes review through the configured external review hook and
maps its result back to approved, rejected, or discarded. That optional path
has its own saved review files; the TUI docs do not promise their retention or
export format.

## 3. MAG human approval

A MAG graph can contain an explicit human approval node. The kernel sends a
correlated `mag.approval_request`; the current starter always presents a live
request through the permission-popup shell, independently of the current
safe/auto/yolo mode. The response is a run-scoped `mag.ApprovalReply` applied to
the requesting gate actor.

- `A` or `Enter` sends approved;
- `D` or `Esc` sends denied with `Denied by user`;
- requests are correlated by run ID and approval ID;
- when a run completes, fails, is killed, or cancels the request, matching
  popups are retracted;
- replayed historical requests do not reopen popups.

This is neither ordinary tool-risk acceptance nor write-review plan approval.
It answers the graph's explicit human checkpoint only. A headless or autonomous
distribution must provide its own actor and policy for these requests; the
starter does not manufacture a human verdict from its permission mode.

## What approval does not guarantee

Approval does not promise rollback, idempotence, exportability, or that later
independent actions are safe. A command may already have performed partial
effects before a workflow is killed. A semantic tool receipt reports observed
invocation/result state; it is not a transaction log.

Mode and verdict authority should not be inferred across process restarts,
session switches, or unsupported cross-minor resumes. See
[sessions and context](sessions.md) and
[workflow termination](workflows.md#sidebar-and-run-inspection).
