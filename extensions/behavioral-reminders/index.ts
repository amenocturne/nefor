/**
 * Behavioral Reminders — Pi Extension
 *
 * Monitors tool_call patterns and sends mid-conversation nudges via
 * sendMessage() when the model drifts from instructions. Reminders are
 * configured in reminders.yaml with cooldowns and per-session caps.
 *
 * Detected patterns:
 * - Write/edit after user asked for plan only
 * - Verbose output (>2000 token response, ~8000 chars)
 * - Premature summary while background tasks still running
 * - Multi-tool attempt (2+ tool calls in one turn)
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { readFileSync } from "fs";
import { dirname, join } from "path";
import { fileURLToPath } from "url";
import { parse } from "yaml";
import { getAllTasks } from "../../lib/task-manager.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));

// ── Types ───────────────────────────────────────────────────────────────

interface ReminderConfig {
  trigger: { type: string; threshold?: number; tools?: string[]; exclude?: string[] };
  message: string;
  cooldown: number;
  max_per_session: number;
}

interface ReminderState {
  lastFiredAt: number; // tool call counter when last fired
  totalFired: number;
}

// ── Tracking State ──────────────────────────────────────────────────────

const WRITE_TOOLS = new Set(["write", "edit"]);

let toolCallCounter = 0;
let planModeActive = false;
const reminderStates = new Map<string, ReminderState>();

// Per-turn text accumulator for verbose output detection
let turnTextBuffer = "";
let turnToolCallCount = 0;

// ── Config Loading ──────────────────────────────────────────────────────

function loadReminders(): Record<string, ReminderConfig> {
  try {
    const raw = readFileSync(join(__dirname, "reminders.yaml"), "utf-8");
    const config = parse(raw);
    return config?.reminders ?? {};
  } catch {
    return {};
  }
}

// ── Helpers ─────────────────────────────────────────────────────────────

function canFire(name: string, config: ReminderConfig): boolean {
  const state = reminderStates.get(name) ?? { lastFiredAt: -999, totalFired: 0 };
  if (state.totalFired >= config.max_per_session) return false;
  if (toolCallCounter - state.lastFiredAt < config.cooldown) return false;
  return true;
}

function recordFire(name: string): void {
  const state = reminderStates.get(name) ?? { lastFiredAt: 0, totalFired: 0 };
  state.lastFiredAt = toolCallCounter;
  state.totalFired++;
  reminderStates.set(name, state);
}

function runningBgTaskCount(): number {
  try {
    return getAllTasks().filter((t) => t.status === "running").length;
  } catch {
    return 0;
  }
}

// ── Extension Entry ─────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
  const reminders = loadReminders();

  function fireReminder(name: string, config: ReminderConfig): void {
    if (!canFire(name, config)) return;
    recordFire(name);
    pi.sendMessage(
      { customType: "behavioral-reminder", content: config.message.trim(), display: true },
    );
  }

  // Detect plan mode from user input
  pi.on("input", async (event) => {
    const text = typeof event === "string" ? event : (event as any).text ?? "";
    const lower = text.toLowerCase();
    if (
      lower.includes("just plan") ||
      lower.includes("plan only") ||
      lower.includes("don't implement") ||
      lower.includes("do not implement") ||
      lower.includes("only plan")
    ) {
      planModeActive = true;
    }
    if (
      lower.includes("go ahead") ||
      lower.includes("proceed") ||
      lower.includes("implement it") ||
      lower.includes("implement this")
    ) {
      planModeActive = false;
    }
    return { action: "continue" as const };
  });

  pi.on("tool_call", async (event) => {
    toolCallCounter++;
    turnToolCallCount++;
    const toolName = event.toolName;

    // Check: write after plan-only
    const planConfig = reminders.write_after_plan_only;
    if (planConfig && planModeActive && WRITE_TOOLS.has(toolName)) {
      fireReminder("write_after_plan_only", planConfig);
    }

    // Check: multi-tool attempt (2+ tool calls in same turn)
    const multiConfig = reminders.multi_tool_attempt;
    if (multiConfig && turnToolCallCount >= 2) {
      fireReminder("multi_tool_attempt", multiConfig);
    }

    return { block: false };
  });

  // Track text output for verbose_output and premature_summary
  pi.on("message_update", async (event) => {
    const delta = (event as any).assistantMessageEvent;
    if (!delta || delta.type !== "text_delta") return;

    const text = typeof delta.delta === "string" ? delta.delta : "";
    turnTextBuffer += text;

    // Check: verbose output (~4 chars per token, threshold in tokens from config)
    const verboseConfig = reminders.verbose_output;
    if (verboseConfig) {
      const charThreshold = (verboseConfig.trigger.threshold ?? 2000) * 4;
      if (turnTextBuffer.length >= charThreshold) {
        fireReminder("verbose_output", verboseConfig);
      }
    }

    // Check: premature summary (model outputs text while bg tasks are running)
    const summaryConfig = reminders.premature_summary;
    if (summaryConfig && runningBgTaskCount() > 0 && turnTextBuffer.length > 100) {
      fireReminder("premature_summary", summaryConfig);
    }

  });

  // Reset per-turn state on turn boundaries
  pi.on("turn_end", async () => {
    turnTextBuffer = "";
    turnToolCallCount = 0;
  });
}
