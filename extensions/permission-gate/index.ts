/**
 * Permission Gate -- Pi extension for tool call interception
 *
 * Behavioral enforcement layer: tool name repair, read-before-edit,
 * no-retry-same-failed-call, plan mode gating. All policy decisions
 * (allow/deny) are delegated to external hooks via lib/hooks.ts.
 *
 * When no hook has an opinion:
 * - Interactive (main session): enqueues into the unified permission queue
 *   for sequential user prompting.
 * - Non-interactive (subagent): writes to the file-based permission queue
 *   for the main session to handle.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { randomUUID } from "crypto";
import { existsSync } from "fs";
import { resolve } from "path";
import {
  loadHooksConfig,
  runHooks,
  buildToolPayload,
  countHooks,
  type HooksConfig,
} from "../../lib/hooks.ts";
import { writeRequest, waitForResponse, type PermissionRequest } from "../../lib/permission-queue.ts";

/** Extension-registered tools that handle their own permissions -- skip the gate */
const PASSTHROUGH_TOOLS = new Set([
  "bg-run", "bg-status", "bg-result", "bg-kill",
  "explore", "write-review", "bg-plan", "progress",
  "wait-explorations", "terminate", "reflect", "read-file", "critique",
]);

// ── Harness Enforcement State ────────────────────────────────────────────

/** Paths the model has read this session -- required before editing */
const readPaths = new Set<string>();

/** Hashes of tool calls that returned errors -- cleared each turn */
const failedCallHashes = new Set<string>();

/** Whether plan mode is active -- blocks write/edit/bash/bg-run */
let planModeActive = false;

/** Tool names blocked during plan mode */
const PLAN_MODE_BLOCKED = new Set(["write", "edit", "bash", "bg-run"]);

/** Recursively sort object keys for stable serialization */
function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === "object") {
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(value as Record<string, unknown>).sort()) {
      sorted[key] = sortKeys((value as Record<string, unknown>)[key]);
    }
    return sorted;
  }
  return value;
}

/** Compute a stable hash key for a tool call */
function callHashKey(toolName: string, input: Record<string, unknown>): string {
  return toolName + ":" + JSON.stringify(sortKeys(input));
}

// ── Tool Call Repair ─────────────────────────────────────────────────────

/**
 * Maps commonly hallucinated tool names to their correct equivalents.
 * Applied after case normalization (all keys are lowercase).
 */
const TOOL_NAME_ALIASES: Record<string, string> = {
  shell: "bash", terminal: "bash", exec: "bash", execute: "bash",
  run_command: "bash", run: "bash", command: "bash",
  search: "grep", find_files: "grep", ripgrep: "grep", rg: "grep",
  glob: "find", list_files: "find",
  file_read: "read", readfile: "read", read_file: "read",
  cat: "read", view: "read", open: "read",
  file_write: "write", writefile: "write", write_file: "write",
  create_file: "write", save: "write",
  file_edit: "edit", editfile: "edit", edit_file: "edit",
  patch: "edit", replace: "edit", modify: "edit",
};

/** Required parameters per built-in tool, used for malformed input feedback */
const TOOL_REQUIRED_PARAMS: Record<string, string[]> = {
  bash: ["command"],
  "bg-run": ["command"],
  read: ["path"],
  write: ["path", "content"],
  edit: ["path", "old_string", "new_string"],
  grep: ["pattern"],
  find: ["pattern"],
  ls: [],
};

/**
 * Normalize a tool name: lowercase + alias resolution.
 * When bash has been replaced by bg-run (background-tasks extension),
 * shell-like aliases resolve to bg-run instead.
 */
function normalizeToolName(raw: string, activeTools: Set<string>): string {
  const lower = raw.toLowerCase().trim();
  const mapped = TOOL_NAME_ALIASES[lower] ?? lower;

  if (mapped === "bash" && !activeTools.has("bash") && activeTools.has("bg-run")) {
    return "bg-run";
  }

  return mapped;
}

/** Check if tool input is missing required parameters */
function findMissingParams(toolName: string, input: Record<string, unknown>): string[] {
  const required = TOOL_REQUIRED_PARAMS[toolName];
  if (!required) return [];
  return required.filter((p) => input[p] === undefined || input[p] === null);
}

/**
 * Attempt tool call repair. Returns a block result if the call should be
 * rejected with feedback, or null if the call is valid and should proceed.
 */
function repairToolCall(
  pi: ExtensionAPI,
  event: { toolName: string; input: unknown },
  ctx: { abort: () => void },
): { block: true; reason: string } | null {
  const raw = event.toolName;
  const activeTools = new Set(pi.getActiveTools());
  const normalized = normalizeToolName(raw, activeTools);
  const input = (event.input ?? {}) as Record<string, unknown>;
  const toolExists = activeTools.has(raw);

  if (!toolExists) {
    if (activeTools.has(normalized)) {
      logInterception(pi, raw, input, "repair", `"${raw}" -> "${normalized}"`);
      ctx.abort();
      return {
        block: true,
        reason:
          `Tool "${raw}" does not exist. You meant "${normalized}". ` +
          `Call the "${normalized}" tool with the same arguments.`,
      };
    }

    const available = [...activeTools].sort().join(", ");
    logInterception(pi, raw, input, "repair", `unknown tool "${raw}"`);
    ctx.abort();
    return {
      block: true,
      reason:
        `Tool "${raw}" does not exist. ` +
        `Available tools: ${available}. ` +
        `Pick the correct tool and try again.`,
    };
  }

  const missing = findMissingParams(raw, input);
  if (missing.length > 0) {
    const allRequired = TOOL_REQUIRED_PARAMS[raw] ?? [];
    logInterception(pi, raw, input, "repair", `missing params: ${missing.join(", ")}`);
    ctx.abort();
    return {
      block: true,
      reason:
        `Invalid input for "${raw}". ` +
        `Missing required parameter${missing.length > 1 ? "s" : ""}: ${missing.join(", ")}. ` +
        `Expected parameters: ${allRequired.join(", ")}. ` +
        `Try again with the correct parameters.`,
    };
  }

  return null;
}

let enqueuePermission: ((toolName: string, toolInput: Record<string, unknown>) => Promise<"allow" | "deny">) | null = null;

/** Allow other extensions to register a permission queue handler */
export function setPermissionQueue(
  handler: (toolName: string, toolInput: Record<string, unknown>) => Promise<"allow" | "deny">,
): void {
  enqueuePermission = handler;
}

export default function (pi: ExtensionAPI) {
  let config: HooksConfig = { hooks: {} };
  let interactive = true;
  let taskId: string | undefined;
  let cwd = ".";

  pi.on("session_start", async (_event, ctx) => {
    cwd = ctx.cwd;
    config = loadHooksConfig(cwd);
    interactive = ctx.hasUI;
    taskId = process.env.NEFOR_TASK_ID;

    const hookCount = countHooks(config);
    const mode = interactive ? "interactive" : taskId ? `subagent (task ${taskId})` : "subagent";

    ctx.ui.notify(
      `Permission Gate: ${hookCount} hook${hookCount !== 1 ? "s" : ""} loaded (${mode})`,
      "info",
    );

    // /noscope (yolo) is on by default — nefor doesn't ask for permission.
    // NEFOR_HARDSCOPE is the explicit opt-out: once set (by /hardscope), it
    // inherits into subagent processes so they don't silently default back
    // to noscope in their own session_start.
    if (!process.env.PI_SKIP_PERMISSIONS && !process.env.NEFOR_HARDSCOPE) {
      process.env.PI_SKIP_PERMISSIONS = "1";
      ctx.ui.notify(
        "we default to /noscope mode (yolo), it's you problem if nefor nukes prod db (use /hardscope if you are scared)",
        "warning",
      );
    }
  });

  pi.registerCommand("noscope", {
    description: "Skip all permission prompts for the rest of this session (default)",
    handler: async (_args, ctx) => {
      delete process.env.NEFOR_HARDSCOPE;
      process.env.PI_SKIP_PERMISSIONS = "1";
      ctx.ui.notify("Noscope active. nefor does what it wants.", "info");
    },
  });

  pi.registerCommand("hardscope", {
    description: "Re-enable permission prompts for the rest of this session",
    handler: async (_args, ctx) => {
      process.env.NEFOR_HARDSCOPE = "1";
      delete process.env.PI_SKIP_PERMISSIONS;
      ctx.ui.notify("Hardscope active. nefor will ask before doing anything scary.", "info");
    },
  });

  pi.on("tool_call", async (event, ctx) => {
    if (process.env.PI_SKIP_PERMISSIONS === "1") {
      return { block: false };
    }

    // Tool call repair -- normalize names and catch hallucinations
    const repairResult = repairToolCall(pi, event, ctx);
    if (repairResult) return repairResult;

    const input = event.input as Record<string, unknown>;

    // ── Harness Enforcement ──────────────────────────────────────────

    // Read-before-edit: block write/edit on files the model hasn't read
    if (event.toolName === "edit" || event.toolName === "write") {
      const filePath = (input.path ?? input.file_path ?? "") as string;
      if (filePath) {
        const resolved = resolve(cwd, filePath);
        const fileExists = existsSync(resolved);
        if (fileExists && !readPaths.has(resolved)) {
          logInterception(pi, event.toolName, input, "enforce", "read-before-edit");
          ctx.abort();
          return {
            block: true,
            reason: `You must read a file before editing it. Call the read tool on '${filePath}' first.`,
          };
        }
      }
    }

    // Track reads
    if (event.toolName === "read") {
      const filePath = (input.path ?? input.file_path ?? "") as string;
      if (filePath) readPaths.add(resolve(cwd, filePath));
    }
    if (event.toolName === "grep") {
      const grepPath = (input.path ?? input.file_path ?? "") as string;
      if (grepPath) readPaths.add(resolve(cwd, grepPath));
    }

    // No-retry-same-failed-call
    const hash = callHashKey(event.toolName, input);
    if (failedCallHashes.has(hash)) {
      logInterception(pi, event.toolName, input, "enforce", "retry-same-failed-call");
      ctx.abort();
      return {
        block: true,
        reason: "This exact tool call already failed. Read the error message and try a different approach.",
      };
    }

    // Plan mode tool gating
    if (planModeActive && PLAN_MODE_BLOCKED.has(event.toolName)) {
      logInterception(pi, event.toolName, input, "enforce", "plan-mode-blocked");
      ctx.abort();
      return {
        block: true,
        reason:
          "Plan mode is active -- write tools are disabled. Propose changes in text instead. " +
          'Say "go ahead" or "implement it" to exit plan mode.',
      };
    }

    // ── End Harness Enforcement ──────────────────────────────────────

    // Extension tools that manage their own permissions
    if (PASSTHROUGH_TOOLS.has(event.toolName)) {
      return { block: false };
    }

    // Run pre_tool_use hooks
    const payload = buildToolPayload(event.toolName, input);
    const hookResults = await runHooks(config, "pre_tool_use", payload, cwd, { toolName: event.toolName });

    for (const result of hookResults) {
      if (result.decision === "deny") {
        failedCallHashes.add(hash);
        logInterception(pi, event.toolName, input, "deny", result.reason);
        ctx.abort();
        return {
          block: true,
          reason: formatDenyReason(result.reason),
        };
      }

      if (result.decision === "allow") {
        logInterception(pi, event.toolName, input, "allow");
        return { block: false };
      }
    }

    // All hooks abstained or none configured -- delegate to user

    if (interactive) {
      if (enqueuePermission) {
        const decision = await enqueuePermission(event.toolName, input);
        logInterception(pi, event.toolName, input, decision, "via permission queue");
        if (decision === "deny") {
          failedCallHashes.add(hash);
          ctx.abort();
          const summary = formatToolSummary(event.toolName, input);
          return { block: true, reason: `The user denied permission for: ${summary}\n\nDo not retry this operation unless the user explicitly asks.` };
        }
        return { block: false };
      }
      const promptResult = await promptUser(pi, ctx, event.toolName, input);
      if (promptResult.block) failedCallHashes.add(hash);
      return promptResult;
    }

    // Non-interactive (subagent): delegate to file-based permission queue
    if (taskId) {
      const reqResult = await requestPermission(pi, ctx, taskId, event.toolName, input);
      if (reqResult.block) failedCallHashes.add(hash);
      return reqResult;
    }

    // No hooks, not interactive, no task ID -- block by default
    failedCallHashes.add(hash);
    logInterception(pi, event.toolName, input, "deny", "No permission source available");
    ctx.abort();
    return {
      block: true,
      reason: "Permission denied: no hooks matched and no interactive session or task ID available",
    };
  });

  // ── Plan Mode Detection ─────────────────────────────────────────────

  let preplanTools: string[] | null = null;

  pi.on("input", async (event) => {
    const text = typeof event === "string" ? event : (event as any).text ?? "";
    const lower = text.toLowerCase();

    const wantsPlan =
      lower.includes("just plan") ||
      lower.includes("plan only") ||
      lower.includes("don't implement") ||
      lower.includes("do not implement") ||
      lower.includes("only plan");

    const wantsImplement =
      lower.includes("go ahead") ||
      lower.includes("proceed") ||
      lower.includes("implement it") ||
      lower.includes("implement this") ||
      lower.includes("do it");

    if (wantsPlan && !planModeActive) {
      planModeActive = true;
      preplanTools = pi.getActiveTools();
      const readOnly = preplanTools.filter((t) => !PLAN_MODE_BLOCKED.has(t));
      pi.setActiveTools(readOnly);
      pi.sendMessage({
        customType: "enforcement",
        content: "Plan mode active. Write tools disabled. Propose changes in text.",
        display: true,
      });
      pi.appendEntry("permission-gate-log", {
        event: "plan-mode-on",
        removedTools: [...PLAN_MODE_BLOCKED].filter((t) => preplanTools!.includes(t)),
      });
    }

    if (wantsImplement && planModeActive) {
      planModeActive = false;
      if (preplanTools) {
        pi.setActiveTools(preplanTools);
        preplanTools = null;
      }
      pi.sendMessage({
        customType: "enforcement",
        content: "Plan mode deactivated. You may now write code.",
        display: true,
      });
      pi.appendEntry("permission-gate-log", { event: "plan-mode-off" });
    }

    return { action: "continue" as const };
  });

  // ── Turn End: reset per-turn enforcement state ──────────────────────

  pi.on("turn_end", async () => {
    failedCallHashes.clear();
  });
}

function formatDenyReason(reason?: string): string {
  const base = reason || "Denied by hook";
  return `Permission denied: ${base}\n\nDO NOT attempt to work around this restriction. Report this block to the user exactly as stated.`;
}

function logInterception(
  pi: ExtensionAPI,
  toolName: string,
  input: Record<string, unknown>,
  decision: string,
  reason?: string,
): void {
  pi.appendEntry("permission-gate-log", { tool: toolName, input, decision, reason });
}

/** Fallback: direct prompt when queue-watcher isn't available */
async function promptUser(
  pi: ExtensionAPI,
  ctx: any,
  toolName: string,
  input: Record<string, unknown>,
): Promise<{ block: boolean; reason?: string }> {
  const summary = formatToolSummary(toolName, input);
  const confirmed = await ctx.ui.confirm(
    "Permission Required",
    `${summary}\n\nAllow this tool call?`,
    { timeout: 60_000 },
  );

  if (confirmed) {
    logInterception(pi, toolName, input, "allow", "user approved");
    return { block: false };
  }

  logInterception(pi, toolName, input, "deny", "user denied");
  ctx.abort();
  return { block: true, reason: `The user denied permission for: ${summary}\n\nDo not retry this operation unless the user explicitly asks.` };
}

/** Subagent mode: write to file queue, wait for main session response */
async function requestPermission(
  pi: ExtensionAPI,
  ctx: any,
  taskId: string,
  toolName: string,
  input: Record<string, unknown>,
): Promise<{ block: boolean; reason?: string }> {
  const requestId = randomUUID();
  const request: PermissionRequest = {
    id: requestId,
    taskId,
    toolName,
    toolInput: input,
    createdAt: new Date().toISOString(),
  };

  writeRequest(request);

  const response = await waitForResponse(taskId, requestId);

  if (!response) {
    logInterception(pi, toolName, input, "deny", "permission queue timeout");
    ctx.abort();
    return { block: true, reason: "Permission denied: no response from main session (timeout)" };
  }

  if (response.decision === "allow") {
    logInterception(pi, toolName, input, "allow", "approved via queue");
    return { block: false };
  }

  const summary = formatToolSummary(toolName, input);
  logInterception(pi, toolName, input, "deny", response.reason || "denied via queue");
  ctx.abort();
  return { block: true, reason: `The user denied permission for: ${summary}\n\nDo not retry this operation unless the user explicitly asks.` };
}

function formatToolSummary(toolName: string, input: Record<string, unknown>): string {
  switch (toolName) {
    case "bash":
      return `bash: ${String(input.command ?? "").slice(0, 200)}`;
    case "read":
      return `read: ${input.path}`;
    case "write":
      return `write: ${input.path}`;
    case "edit":
      return `edit: ${input.path}`;
    case "grep":
      return `grep: ${input.pattern} in ${input.path || "."}`;
    case "find":
      return `find: ${input.path || "."}`;
    case "ls":
      return `ls: ${input.path || "."}`;
    default:
      return `${toolName}: ${JSON.stringify(input).slice(0, 200)}`;
  }
}
