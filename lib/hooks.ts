/**
 * Centralized Hook Runner
 *
 * Loads hook config from .pi/hooks.yaml or .pi/hooks.json, runs hook commands
 * as subprocesses, and returns their results. Generic — no knowledge of
 * specific hook tools. All policy decisions are delegated to external hooks.
 */

import { spawn } from "child_process";
import { existsSync, readFileSync } from "fs";
import { dirname, join } from "path";

// ── Types ───────────────────────────────────────────────────────────────

export type HookEntry = {
  command: string;
  timeout?: number;
};

export type HooksConfig = {
  hooks: {
    pre_tool_use?: HookEntry[];
    post_tool_use?: HookEntry[];
    on_plan_ready?: HookEntry[];
    on_task_complete?: HookEntry[];
  };
};

export type HookResult = {
  decision: "allow" | "deny" | "abstain";
  reason?: string;
};

export type PlanHookResult = {
  status: "approved" | "changes_needed" | "abstain";
  comments?: string;
};

// ── Config Loading ──────────────────────────────────────────────────────

/**
 * Try to parse YAML. Uses a minimal inline parser for the simple hooks
 * structure to avoid requiring an external yaml dependency at runtime.
 * Falls back to JSON parse if it looks like JSON.
 */
const tryParseYaml = (raw: string): unknown => {
  // If it starts with { it's likely JSON
  if (raw.trimStart().startsWith("{")) return JSON.parse(raw);

  // Minimal YAML parser for the hooks config structure:
  // hooks:
  //   pre_tool_use:
  //     - command: "..."
  //       timeout: 5000
  const result: Record<string, Record<string, Array<Record<string, unknown>>>> = {};
  let currentTopKey = "";
  let currentListKey = "";
  let currentItem: Record<string, unknown> | null = null;

  for (const line of raw.split("\n")) {
    const stripped = line.trimEnd();
    if (!stripped || stripped.startsWith("#")) continue;

    // Top-level key (e.g., "hooks:")
    const topMatch = stripped.match(/^(\w+):\s*$/);
    if (topMatch) {
      currentTopKey = topMatch[1];
      result[currentTopKey] = {};
      currentListKey = "";
      currentItem = null;
      continue;
    }

    // Second-level key (e.g., "  pre_tool_use:")
    const secondMatch = stripped.match(/^\s{2}(\w+):\s*$/);
    if (secondMatch && currentTopKey) {
      currentListKey = secondMatch[1];
      result[currentTopKey][currentListKey] = [];
      currentItem = null;
      continue;
    }

    // List item start (e.g., "    - command: ...")
    const listItemMatch = stripped.match(/^\s{4}-\s+(\w+):\s*(.+)/);
    if (listItemMatch && currentTopKey && currentListKey) {
      currentItem = { [listItemMatch[1]]: parseYamlValue(listItemMatch[2]) };
      result[currentTopKey][currentListKey].push(currentItem);
      continue;
    }

    // Continuation of list item (e.g., "      timeout: 5000")
    const contMatch = stripped.match(/^\s{6}(\w+):\s*(.+)/);
    if (contMatch && currentItem) {
      currentItem[contMatch[1]] = parseYamlValue(contMatch[2]);
      continue;
    }
  }

  return result;
};

const parseYamlValue = (raw: string): string | number | boolean => {
  const trimmed = raw.trim();
  // Strip surrounding quotes
  if ((trimmed.startsWith('"') && trimmed.endsWith('"')) ||
      (trimmed.startsWith("'") && trimmed.endsWith("'"))) {
    return trimmed.slice(1, -1);
  }
  if (trimmed === "true") return true;
  if (trimmed === "false") return false;
  const num = Number(trimmed);
  if (!isNaN(num) && trimmed !== "") return num;
  return trimmed;
};

/**
 * Load hook configuration from .pi/hooks.yaml or .pi/hooks.json.
 * YAML takes precedence. Returns empty config if neither exists.
 */
export const loadHooksConfig = (cwd: string): HooksConfig => {
  const yamlPath = join(cwd, ".pi", "hooks.yaml");
  const jsonPath = join(cwd, ".pi", "hooks.json");
  const empty: HooksConfig = { hooks: {} };

  const configPath = existsSync(yamlPath) ? yamlPath : existsSync(jsonPath) ? jsonPath : null;
  if (!configPath) return empty;

  try {
    const hookDir = dirname(configPath);
    const raw = readFileSync(configPath, "utf-8").replaceAll("{hook_dir}", hookDir);
    const parsed = tryParseYaml(raw) as Record<string, unknown>;

    if (typeof parsed !== "object" || parsed === null) return empty;

    // Handle both flat format (hooks: { pre_tool_use: [...] }) and
    // Claude Code format (PreToolUse: [{ matcher, hooks }])
    if ("hooks" in parsed && typeof parsed.hooks === "object") {
      return parsed as HooksConfig;
    }

    // Legacy Claude Code format — convert PreToolUse matcher groups
    if ("PreToolUse" in parsed) {
      return convertLegacyConfig(parsed);
    }

    return empty;
  } catch {
    return empty;
  }
};

/** Convert the old Claude Code hooks.json format to the new flat format */
const convertLegacyConfig = (parsed: Record<string, unknown>): HooksConfig => {
  const groups = parsed.PreToolUse as Array<{ matcher: string; hooks: Array<{ command: string; timeout?: number }> }>;
  if (!Array.isArray(groups)) return { hooks: {} };

  // Flatten all hooks from all matcher groups into pre_tool_use.
  // Store the matcher pattern on each entry so runHooks can filter.
  const entries: Array<HookEntry & { _matcher?: string }> = [];
  for (const group of groups) {
    for (const hook of group.hooks ?? []) {
      entries.push({ ...hook, _matcher: group.matcher });
    }
  }

  return { hooks: { pre_tool_use: entries } };
};

// ── Hook Execution ──────────────────────────────────────────────────────

const DEFAULT_TIMEOUT_MS = 30_000;

/** Tool name capitalization for the hook protocol (hooks expect PascalCase) */
const TOOL_NAME_MAP: Record<string, string> = {
  bash: "Bash", read: "Read", write: "Write", edit: "Edit",
  grep: "Grep", find: "Find", ls: "Ls",
};

const capitalizeToolName = (name: string): string =>
  TOOL_NAME_MAP[name] ?? name.charAt(0).toUpperCase() + name.slice(1);

/** Interpolate {key} placeholders in a command string from a payload */
const interpolateCommand = (cmd: string, payload: Record<string, unknown>): string =>
  cmd.replace(/\{(\w+)\}/g, (match, key) => {
    const val = payload[key];
    return val !== undefined ? String(val) : match;
  });

/** Run a single hook subprocess. Returns the parsed result or abstain on failure. */
const runSingleHook = (
  entry: HookEntry,
  payload: Record<string, unknown>,
  cwd: string,
): Promise<Record<string, unknown> | null> =>
  new Promise((resolve) => {
    const timeoutMs = entry.timeout ?? DEFAULT_TIMEOUT_MS;
    const cmd = interpolateCommand(entry.command, payload);
    const parts = cmd.split(/\s+/);
    const bin = parts[0];
    const args = parts.slice(1);

    let child: ReturnType<typeof spawn>;
    try {
      child = spawn(bin, args, {
        cwd,
        stdio: ["pipe", "pipe", "pipe"],
        env: { ...process.env },
      });
    } catch {
      resolve(null);
      return;
    }

    let stdout = "";
    let settled = false;

    const finish = (result: Record<string, unknown> | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(result);
    };

    const timer = setTimeout(() => {
      if (!settled) {
        try { child.kill("SIGKILL"); } catch {}
        finish(null);
      }
    }, timeoutMs);

    child.stdout!.on("data", (chunk: Buffer) => { stdout += chunk.toString(); });
    child.on("error", () => finish(null));

    child.on("close", (code) => {
      if (code !== 0) { finish(null); return; }
      const trimmed = stdout.trim();
      if (!trimmed) { finish(null); return; }
      try {
        finish(JSON.parse(trimmed));
      } catch {
        finish(null);
      }
    });

    try {
      child.stdin!.write(JSON.stringify(payload));
      child.stdin!.end();
    } catch {
      finish(null);
    }
  });

// ── Public API ──────────────────────────────────────────────────────────

/**
 * Check whether a hook entry matches a given tool name.
 * Supports the legacy _matcher field (pipe-separated, case-insensitive).
 * Entries without a matcher match all tools.
 */
const hookMatchesTool = (entry: HookEntry & { _matcher?: string }, toolName: string): boolean => {
  const matcher = (entry as any)._matcher;
  if (!matcher) return true;
  const matchers = String(matcher).split("|").map(m => m.toLowerCase());
  return matchers.includes(toolName.toLowerCase());
};

/**
 * Run all hooks for a given hook point. Returns results in order.
 * Each hook runs sequentially (for pre_tool_use, deny takes priority).
 */
export const runHooks = async (
  config: HooksConfig,
  hookPoint: keyof HooksConfig["hooks"],
  payload: Record<string, unknown>,
  cwd: string,
  opts?: { toolName?: string },
): Promise<HookResult[]> => {
  const entries = config.hooks[hookPoint] ?? [];
  if (entries.length === 0) return [];

  const results: HookResult[] = [];

  for (const entry of entries) {
    // For pre_tool_use, filter by tool name matcher if present
    if (hookPoint === "pre_tool_use" && opts?.toolName && !hookMatchesTool(entry, opts.toolName)) {
      continue;
    }

    const raw = await runSingleHook(entry, payload, cwd);

    if (!raw) {
      results.push({ decision: "abstain" });
      continue;
    }

    // Parse pre_tool_use protocol (hookSpecificOutput wrapper)
    const output = (raw as any)?.hookSpecificOutput;
    if (output?.permissionDecision) {
      const decision = output.permissionDecision;
      if (decision === "allow") {
        results.push({ decision: "allow" });
      } else if (decision === "deny") {
        results.push({ decision: "deny", reason: output.permissionDecisionReason ?? "Denied by hook" });
      } else {
        results.push({ decision: "abstain" });
      }
      continue;
    }

    // Direct decision format (simpler protocol)
    if ("decision" in raw) {
      const d = raw.decision as string;
      if (d === "allow" || d === "deny" || d === "abstain") {
        results.push({ decision: d, reason: raw.reason as string | undefined });
        continue;
      }
    }

    results.push({ decision: "abstain" });
  }

  return results;
};

/**
 * Run on_plan_ready hooks. Returns the first non-abstain result, or null
 * if no hooks are configured or all abstain.
 */
export const runPlanHooks = async (
  config: HooksConfig,
  payload: { plan_path: string; content: string },
  cwd: string,
): Promise<PlanHookResult | null> => {
  const entries = config.hooks.on_plan_ready ?? [];
  if (entries.length === 0) return null;

  for (const entry of entries) {
    const raw = await runSingleHook(entry, payload as unknown as Record<string, unknown>, cwd);
    if (!raw) continue;

    const status = raw.status as string | undefined;
    if (status === "approved" || status === "changes_needed") {
      return { status, comments: raw.comments as string | undefined };
    }
  }

  return null;
};

/**
 * Run informational hooks (post_tool_use, on_task_complete). Fire-and-forget.
 * Errors are silently ignored.
 */
export const runInfoHooks = (
  config: HooksConfig,
  hookPoint: "post_tool_use" | "on_task_complete",
  payload: Record<string, unknown>,
  cwd: string,
): void => {
  const entries = config.hooks[hookPoint] ?? [];
  for (const entry of entries) {
    runSingleHook(entry, payload, cwd).catch(() => {});
  }
};

/** Build the pre_tool_use payload in the expected protocol format */
export const buildToolPayload = (
  toolName: string,
  toolInput: Record<string, unknown>,
): Record<string, unknown> => ({
  tool_name: capitalizeToolName(toolName),
  tool_input: toolInput,
});

/** Count total configured hooks across all hook points */
export const countHooks = (config: HooksConfig): number => {
  const h = config.hooks;
  return (h.pre_tool_use?.length ?? 0)
    + (h.post_tool_use?.length ?? 0)
    + (h.on_plan_ready?.length ?? 0)
    + (h.on_task_complete?.length ?? 0);
};
