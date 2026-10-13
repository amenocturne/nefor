# Permissions

Permission mode and approval kind are separate. The starter has **three
user-facing approval systems**:

1. ordinary tool permission, decided from the agent's approval policy and the mode;
2. write-review plan decisions;
3. explicit MAG human-approval nodes.

The mode (`/safe`, `/auto`, `/yolo`) takes part in the first two. MAG human nodes remain
explicitly interactive in the current starter; they do not inherit an automatic
mode verdict.

## Approval policy and mode

What an agent may do with its tools is its **approval policy**, written on the
agent in MAG as `tool_approval_policy`. There are two, in order:

- `ToolApprovalPolicy.ReadOnly` — the agent may only read. Anything else is
  rejected, in every mode, and the agent is never asked about.
- `ToolApprovalPolicy.Default` — the configured behaviour for the current mode.

The policy is the only source of truth. It is not derived from the agent's
tool list: an agent built with `nefor.read_only_tools` and `Default` is a
`Default` agent with few tools.

The **mode** is the user's choice for the session: `/safe`, `/auto`, or
`/yolo`. At process start, `nefor run --mode safe|auto|yolo` selects the
initial mode and `--yolo` aliases `--mode yolo`. Repeated startup mode controls
are interpreted in argument order, so the last wins. Without an explicit
startup mode, startup and resume are safe; mode is not restored as durable
authority from a prior process.

## 1. Ordinary tool permission

Every tool call goes through the `tool-gate`. The tool itself says what the
call means under the caller's policy: **safe**, **unsure** (it could not be
shown to be safe), or **denied**. The gate then decides, policy first and mode
second:

| The tool says | ReadOnly agent (every mode) | Default agent, `safe` | Default, `auto` | Default, `yolo` |
| ------------- | --------------------------- | --------------------- | --------------- | --------------- |
| safe          | runs                        | runs                  | runs            | runs            |
| unsure        | rejected                    | you are asked         | runs            | runs            |
| denied        | rejected                    | rejected              | rejected        | runs            |

`yolo` is dangerous for `Default` agents and changes nothing for `ReadOnly`
agents. A rejected call returns an error that tells the model why, for example
``tool `write_file` was not run: your approval policy is ReadOnly and it
writes a file``.

When an invocation has a tool allowlist, the requested tool must be in it;
otherwise the call is answered as an unknown tool in every mode.

### What the starter's tools say

| Tool | ReadOnly | Default |
| ---- | -------- | ------- |
| `read_file`, `read_image`, `discover_instruction_files` | safe | configured: automatic |
| `mag-status`, `mag-await`, `mag-terminate`, `mag-write-file`, `mag-preview` | safe | configured: automatic |
| `git_worktree_open` | safe | configured: asks |
| `write_file` | denied: it writes a file | safe with a current approved write-review plan, otherwise denied |
| `write-review` | denied: it asks you to approve a plan to write | configured: automatic |
| `git_worktree_create` | denied: it creates a worktree and a branch | configured: asks |
| `shell.script`, `process.exec` | what the command classifier says about the command, with the strict read-only flags | what the classifier says, with the wide flags |
| `mag-apply` | the workflow as a whole, see below | the workflow as a whole |

"Configured" is `config.active.tool_gate`: a tool in `auto_tools` is safe, any
other tool is unsure. Commands are classified by `da`: it approves (safe),
defers (unsure), or denies. A missing or unusable `da` denies with an
actionable reason. `process.exec` arguments are written as the equivalent
quoted shell command and classified the same way.

### Workflows are approved as a unit

Agents run commands through MAG workflows. When an agent applies a workflow
with `mag-apply`, the whole compiled workflow is assessed under that agent's
policy before anything runs: every script and process node's command, every
worktree node, and every agent node. An agent node with a wider policy than
the applying agent's is a rejection. One node that is not safe decides the
workflow, and the error lists all of them.

So a `ReadOnly` agent can search with commands, and a workflow of its that
would write is rejected with the reason. For a `Default` agent in `safe`, a
workflow with a command the classifier is unsure about asks you once, showing
which nodes; approving it approves that workflow. Once a workflow runs, its
nodes are not asked about one by one. This applies to every workflow an agent
applies, including one applied by an agent inside another workflow. The lead's
own turn program is part of the configuration, not something a model applies.

For an ordinary permission popup, `A` or `Enter` approves; `D` or `Esc` denies.
Multiple requests queue rather than replacing the active popup. Approval
authorizes that call; it is not a blanket plan approval. In the headless CLI
nobody can be asked: a question is answered with a denial.

## 2. Write-review plan approval

`write-review` is a lead-workflow tool. Calling it does not
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
