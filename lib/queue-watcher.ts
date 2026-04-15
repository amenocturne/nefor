/**
 * Queue Watcher — Unified permission queue
 *
 * Single sequential queue for ALL permission requests — from the main agent's
 * own tools, from subagent file-based requests, and from background commands.
 * Pi's UI can only show one prompt at a time, so requests are processed
 * one-by-one in FIFO order regardless of source.
 *
 * Main agent tools:  permission-gate → enqueuePermission() → prompt → resolve
 * Subagent tools:    file queue → pollOnce() → enqueuePermission() → prompt → file response
 */

import type { ExtensionContext } from "@mariozechner/pi-coding-agent";
import {
  cleanupAll,
  scanRequests,
  writeResponse,
  type PermissionResponse,
} from "./permission-queue.ts";
import { getTask } from "./task-manager.ts";

// ── Types ───────────────────────────────────────────────────────────────

interface QueueEntry {
  toolName: string;
  toolInput: Record<string, unknown>;
  source: "main" | "subagent";
  /** For subagent requests: task ID and request ID for file response */
  taskId?: string;
  requestId?: string;
  /** Resolve the caller's promise with the decision */
  resolve: (decision: "allow" | "deny") => void;
}

// ── State ───────────────────────────────────────────────────────────────

let watchInterval: ReturnType<typeof setInterval> | null = null;
let extensionCtx: ExtensionContext | null = null;
const handledRequests = new Set<string>();
const queue: QueueEntry[] = [];
let processing = false;

// ── Public API ──────────────────────────────────────────────────────────

export function startWatching(ctx: ExtensionContext): void {
  extensionCtx = ctx;
  if (watchInterval) return;

  watchInterval = setInterval(() => {
    pollSubagentRequests();
    processNext();
  }, 500);
}

export function stopWatching(): void {
  if (watchInterval) {
    clearInterval(watchInterval);
    watchInterval = null;
  }
  handledRequests.clear();
  queue.length = 0;
  processing = false;
  extensionCtx = null;
}

export function cleanupQueue(): void {
  stopWatching();
  cleanupAll();
}

/**
 * Enqueue a permission request from the main agent's permission-gate.
 * Returns a promise that resolves with "allow" or "deny" when the user responds.
 */
export function enqueuePermission(
  toolName: string,
  toolInput: Record<string, unknown>,
): Promise<"allow" | "deny"> {
  return new Promise((resolve) => {
    queue.push({ toolName, toolInput, source: "main", resolve });
    processNext();
  });
}

// ── Subagent File Queue Polling ─────────────────────────────────────────

function pollSubagentRequests(): void {
  const requests = scanRequests();

  for (const req of requests) {
    const key = `${req.taskId}:${req.id}`;
    if (handledRequests.has(key)) continue;
    handledRequests.add(key);

    const task = getTask(req.taskId);
    if (task) {
      task.pendingPermissions++;
    }

    queue.push({
      toolName: req.toolName,
      toolInput: req.toolInput,
      source: "subagent",
      taskId: req.taskId,
      requestId: req.id,
      resolve: () => {}, // resolved via file response
    });
  }
}

// ── Sequential Processing ───────────────────────────────────────────────

async function processNext(): Promise<void> {
  if (processing || queue.length === 0 || !extensionCtx) return;

  processing = true;

  while (queue.length > 0) {
    const entry = queue.shift()!;
    await handleEntry(extensionCtx, entry);
  }

  processing = false;
}

async function handleEntry(
  ctx: ExtensionContext,
  entry: QueueEntry,
): Promise<void> {
  const summary = formatToolSummary(entry.toolName, entry.toolInput);
  const sourceLabel = entry.source === "subagent" ? ` (subagent ${entry.taskId})` : "";
  const queuedCount = queue.length;
  const queueHint = queuedCount > 0 ? ` [+${queuedCount} queued]` : "";

  let decision: "allow" | "deny" = "deny";

  if (ctx.hasUI) {
    const confirmed = await ctx.ui.confirm(
      `${entry.toolName}${sourceLabel}${queueHint}`,
      summary,
      { timeout: 120_000 },
    );
    decision = confirmed ? "allow" : "deny";
  }

  // Resolve the caller's promise (main agent permission-gate)
  entry.resolve(decision);

  // Write file response for subagent requests
  if (entry.source === "subagent" && entry.taskId && entry.requestId) {
    const response: PermissionResponse = {
      id: entry.requestId,
      decision,
      respondedAt: new Date().toISOString(),
    };
    writeResponse(response, entry.taskId);

    const task = getTask(entry.taskId);
    if (task && task.pendingPermissions > 0) {
      task.pendingPermissions--;
    }
  }
}

// ── Helpers ─────────────────────────────────────────────────────────────

function formatToolSummary(
  toolName: string,
  toolInput: Record<string, unknown>,
): string {
  switch (toolName) {
    case "bash":
      return truncate(String(toolInput.command ?? ""), 120);
    case "read":
      return truncate(String(toolInput.file_path ?? toolInput.path ?? ""), 120);
    case "write":
    case "edit":
      return truncate(String(toolInput.file_path ?? toolInput.path ?? ""), 120);
    case "grep":
    case "find":
      return truncate(String(toolInput.pattern ?? toolInput.path ?? ""), 120);
    case "ls":
      return truncate(String(toolInput.path ?? "."), 120);
    default:
      return truncate(JSON.stringify(toolInput), 120);
  }
}

function truncate(s: string, maxLen: number): string {
  if (s.length <= maxLen) return s;
  return s.slice(0, maxLen - 3) + "...";
}
