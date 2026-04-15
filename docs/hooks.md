# Nefor Hook Points

Hooks are external commands invoked at specific points in the agent lifecycle.
Configure in `.pi/hooks.yaml` (or `.pi/hooks.json`).

## pre_tool_use

Fires before any tool execution. Hook receives tool call info on stdin as JSON.

**Input:**
```json
{ "tool_name": "Bash", "tool_input": { "command": "git status" } }
```

**Returns:**
```json
{ "hookSpecificOutput": { "permissionDecision": "allow" | "deny" | "abstain", "permissionDecisionReason": "..." } }
```

**Resolution logic:**
- If any hook returns `deny`: tool call is blocked.
- If any hook returns `allow` and none deny: tool call proceeds without prompting.
- If no hooks configured or all abstain: nefor prompts the user (default behavior).

## post_tool_use

Fires after tool execution completes. Informational only -- cannot block.

**Input:**
```json
{ "tool": "Bash", "input": { "command": "git status" }, "output": "...", "duration_ms": 150 }
```

No return value expected. Non-zero exit is silently ignored.

## on_plan_ready

Fires when the orchestrator has a plan ready for review.

**Input:**
```json
{ "plan_path": "/path/to/plan.md", "content": "..." }
```

**Returns:**
```json
{ "status": "approved" | "changes_needed", "comments": "..." }
```

**Resolution logic:**
- If no hook configured or all abstain: nefor displays plan inline and waits for user to approve/request changes via interactive prompt.
- If hook returns a result: use it directly (approved or changes_needed + comments).

## on_task_complete

Fires when a DAG node completes (success or failure). Informational only.

**Input:**
```json
{ "node_id": "build-auth", "agent_type": "builder", "status": "done" | "error", "output": "..." }
```

No return value expected. For notifications, logging, external integrations.

## Configuration

### `.pi/hooks.yaml`

```yaml
hooks:
  pre_tool_use:
    - command: "smart-approve"
      timeout: 5000
    - command: "deny-read"
      timeout: 5000
  on_plan_ready:
    - command: "review-tool launch --file {plan_path}"
      timeout: 300000
  on_task_complete:
    - command: "notification send --title 'Task: {node_id}'"
```

### `.pi/hooks.json`

Same structure, JSON format. Both files are checked; YAML takes precedence.

## Protocol

- **stdin**: JSON payload (see input format per hook point)
- **stdout**: JSON response (see return format per hook point)
- **Timeout**: configurable per hook entry, default 30s
- **Non-zero exit or timeout**: treated as `abstain` (no opinion)
- **No stdout**: treated as `abstain`
- **`{hook_dir}`** in command strings is replaced with the directory containing the hooks config file
- **Template variables** like `{plan_path}`, `{node_id}` in command strings are replaced from the payload
