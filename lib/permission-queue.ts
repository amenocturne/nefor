/**
 * Permission Queue — File-based IPC
 *
 * Atomic read/write logic for permission request/response files.
 * Used by:
 *   - permission-gate (write side, in subagents) — writes .request.json, polls for .response.json
 *   - queue-watcher (read side, in main session) — watches for .request.json, writes .response.json
 *
 * All writes are atomic: write to .tmp, then rename. This prevents partial reads.
 */

import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "fs";
import { homedir } from "os";
import { join } from "path";

// ── Types ───────────────────────────────────────────────────────────────

export interface PermissionRequest {
  id: string;
  taskId: string;
  toolName: string;
  toolInput: Record<string, unknown>;
  createdAt: string;
}

export interface PermissionResponse {
  id: string;
  decision: "allow" | "deny";
  reason?: string;
  respondedAt: string;
}

export const QUEUE_BASE_DIR = ".pi/agent/permission-queue";

// ── Paths ────────────────────────────────────────────────────────────

function queueRoot(): string {
  return join(homedir(), QUEUE_BASE_DIR);
}

function taskDir(taskId: string): string {
  return join(queueRoot(), taskId);
}

function requestPath(taskId: string, requestId: string): string {
  return join(taskDir(taskId), `${requestId}.request.json`);
}

function responsePath(taskId: string, requestId: string): string {
  return join(taskDir(taskId), `${requestId}.response.json`);
}

// ── Atomic Write ─────────────────────────────────────────────────────

function atomicWrite(filePath: string, data: unknown): void {
  const tmp = `${filePath}.tmp`;
  writeFileSync(tmp, JSON.stringify(data, null, 2), "utf-8");
  renameSync(tmp, filePath);
}

// ── Write Side (subagent permission-gate) ────────────────────────────

export function writeRequest(request: PermissionRequest): string {
  const dir = taskDir(request.taskId);
  mkdirSync(dir, { recursive: true });
  const path = requestPath(request.taskId, request.id);
  atomicWrite(path, request);
  return path;
}

export function readResponse(
  taskId: string,
  requestId: string,
): PermissionResponse | null {
  const path = responsePath(taskId, requestId);
  if (!existsSync(path)) return null;
  try {
    const raw = readFileSync(path, "utf-8");
    return JSON.parse(raw) as PermissionResponse;
  } catch {
    return null;
  }
}

export async function waitForResponse(
  taskId: string,
  requestId: string,
  intervalMs = 150,
  timeoutMs = 120_000,
): Promise<PermissionResponse | null> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const resp = readResponse(taskId, requestId);
    if (resp) return resp;
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
  return null;
}

// ── Read Side (main session) ────────────────────────────────────────

export function scanRequests(filterTaskId?: string): PermissionRequest[] {
  const root = queueRoot();
  if (!existsSync(root)) return [];

  const requests: PermissionRequest[] = [];
  const taskDirs = filterTaskId
    ? [filterTaskId]
    : readdirSync(root).filter((f) => {
        try { return existsSync(join(root, f)); } catch { return false; }
      });

  for (const tid of taskDirs) {
    const dir = join(root, tid);
    if (!existsSync(dir)) continue;

    let files: string[];
    try { files = readdirSync(dir); } catch { continue; }

    for (const file of files) {
      if (!file.endsWith(".request.json")) continue;
      const reqId = file.replace(".request.json", "");
      if (files.includes(`${reqId}.response.json`)) continue;

      try {
        const raw = readFileSync(join(dir, file), "utf-8");
        requests.push(JSON.parse(raw) as PermissionRequest);
      } catch {}
    }
  }

  return requests;
}

export function writeResponse(response: PermissionResponse, taskId: string): void {
  const dir = taskDir(taskId);
  mkdirSync(dir, { recursive: true });
  const path = responsePath(taskId, response.id);
  atomicWrite(path, response);
}

// ── Cleanup ──────────────────────────────────────────────────────────

export function cleanupTask(taskId: string): void {
  const dir = taskDir(taskId);
  if (!existsSync(dir)) return;
  try {
    for (const file of readdirSync(dir)) {
      try { unlinkSync(join(dir, file)); } catch {}
    }
    try { rmSync(dir, { recursive: true, force: true }); } catch {}
  } catch {}
}

export function cleanupAll(): void {
  const root = queueRoot();
  if (!existsSync(root)) return;
  try { rmSync(root, { recursive: true, force: true }); } catch {}
}
