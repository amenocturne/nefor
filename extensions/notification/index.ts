/**
 * Notification — Pi extension for system notifications
 *
 * Reads Notification/Stop hook entries from .pi/hooks.json (written by
 * the installer) and fires them when:
 * - The agent is idle and waiting for user input (30s threshold)
 * - The session shuts down
 * - A background task completes (if background-tasks is loaded)
 *
 * The actual notification delivery (macOS terminal-notifier, Linux
 * notify-send) is handled by the hook script — this extension only
 * decides *when* to fire.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { spawn } from "child_process";
import { existsSync, readFileSync } from "fs";
import { join } from "path";

// ── Types ──────────────────────────────────────────────────────────────

interface HookEntry {
  type: string;
  command: string;
  timeout?: number;
}

interface MatcherGroup {
  matcher?: string;
  hooks: HookEntry[];
}

interface HookConfig {
  Stop?: MatcherGroup[];
  Notification?: MatcherGroup[];
}

// ── Hook execution ─────────────────────────────────────────────────────

function loadConfig(piDir: string): HookConfig {
  const hooksPath = join(piDir, "hooks.json");
  if (!existsSync(hooksPath)) return {};
  try {
    return JSON.parse(readFileSync(hooksPath, "utf-8"));
  } catch {
    return {};
  }
}

function fireHook(entry: HookEntry, payload: Record<string, unknown>, cwd: string): void {
  const parts = entry.command.split(/\s+/);
  const cmd = parts[0];
  const args = parts.slice(1);
  const timeoutMs = (entry.timeout || 5) * 1000;

  try {
    const child = spawn(cmd, args, {
      cwd,
      stdio: ["pipe", "ignore", "ignore"],
      env: { ...process.env },
    });

    const timer = setTimeout(() => {
      try { child.kill("SIGKILL"); } catch {}
    }, timeoutMs);

    child.on("close", () => clearTimeout(timer));
    child.on("error", () => clearTimeout(timer));

    child.stdin!.write(JSON.stringify(payload));
    child.stdin!.end();
  } catch {}
}

function fireAll(groups: MatcherGroup[] | undefined, payload: Record<string, unknown>, cwd: string): void {
  if (!groups) return;
  for (const group of groups) {
    for (const hook of group.hooks) {
      fireHook(hook, payload, cwd);
    }
  }
}

// ── Extension ──────────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
  let config: HookConfig = {};
  let cwd = ".";
  let lastActivityTime = Date.now();
  let idleNotified = false;
  let idleCheckInterval: ReturnType<typeof setInterval> | null = null;

  const IDLE_THRESHOLD_MS = 30_000;

  pi.on("session_start", async (_event, ctx) => {
    cwd = ctx.cwd;
    config = loadConfig(join(cwd, ".pi"));
    lastActivityTime = Date.now();
    idleNotified = false;

    // Poll for idle state — agent finished and is waiting for user
    if (idleCheckInterval) clearInterval(idleCheckInterval);
    idleCheckInterval = setInterval(() => {
      if (!ctx.isIdle()) {
        lastActivityTime = Date.now();
        idleNotified = false;
        return;
      }
      if (!idleNotified && Date.now() - lastActivityTime > IDLE_THRESHOLD_MS) {
        idleNotified = true;
        fireAll(config.Notification, {
          hook_event_name: "Notification",
          notification_type: "idle_prompt",
        }, cwd);
      }
    }, 5_000);
  });

  // Reset idle tracking on user input
  pi.on("input", async () => {
    lastActivityTime = Date.now();
    idleNotified = false;
    return { action: "continue" as const };
  });

  // Reset idle tracking on agent activity
  pi.on("agent_start", async () => {
    lastActivityTime = Date.now();
    idleNotified = false;
  });

  // Agent finished work — start idle countdown
  pi.on("agent_end", async () => {
    lastActivityTime = Date.now();
  });

  // Fire Stop notification on session shutdown
  pi.on("session_shutdown", async () => {
    if (idleCheckInterval) { clearInterval(idleCheckInterval); idleCheckInterval = null; }
    fireAll(config.Stop, { hook_event_name: "Stop" }, cwd);
  });
}
