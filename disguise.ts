import { defineDisguise, type Effect, type WorkflowContext, type AgentConfig } from "./lib/workflow/index.ts";
import { killTask, killAllTasks, getTask } from "./lib/task-manager.ts";
import { loadHooksConfig, runPlanHooks } from "./lib/hooks.ts";
import { execSync } from "child_process";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "fs";
import { resolve, join } from "path";
import config from "./config/index.ts";

// ── Agent Configs ───────────────────────────────────────────────────────

const explorer: AgentConfig = {
  provider: config.provider,
  model: config.models.explorer,
  prompt: "prompts/explorer.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

const builder: AgentConfig = {
  provider: config.provider,
  model: config.models.worker,
  prompt: "prompts/builder.md",
  tools: { allow: ["read", "write", "edit", "bash", "grep", "find", "ls", "glob"] },
};

const reviewer: AgentConfig = {
  provider: config.provider,
  model: config.models.reviewer,
  prompt: "prompts/reviewer.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

const tester: AgentConfig = {
  provider: config.provider,
  model: config.models.tester,
  prompt: "prompts/tester.md",
  tools: { allow: ["read", "bash", "grep", "find", "ls"] },
};

const reflector: AgentConfig = {
  provider: config.provider,
  model: config.models.explorer,
  prompt: "prompts/reflector.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

const promptEngineer: AgentConfig = {
  provider: config.provider,
  model: config.models.promptEngineer,
  prompt: "prompts/prompt-engineer.md",
  tools: { allow: ["read", "write", "edit", "grep", "find", "ls", "glob"] },
};

const critic: AgentConfig = {
  provider: config.provider,
  model: config.models.orchestrator,
  prompt: "prompts/critic.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

const agentConfigs: Record<string, AgentConfig> = {
  builder,
  reviewer,
  tester,
  explorer,
  critic,
  reflector,
  "prompt-engineer": promptEngineer,
};

// ── Types ───────────────────────────────────────────────────────────────

type DagNode = {
  id: string;
  description: string;
  agentType: string;
  dependencies: string[];
  workDir: string;
  status: "pending" | "running" | "done" | "failed";
  attempts: number;
  maxAttempts: number;
  startedAt?: number;
  finishedAt?: number;
  processId?: string;
  output?: string;
  summary?: string;
};


// ── Helpers ─────────────────────────────────────────────────────────────

const getReadyNodes = (state: Record<string, unknown>): DagNode[] => {
  const nodes = state.nodes as Map<string, DagNode>;
  const ready: DagNode[] = [];
  for (const node of nodes.values()) {
    if (node.status !== "pending") continue;
    const depsmet = node.dependencies.every(dep => {
      const depNode = nodes.get(dep);
      return depNode && depNode.status === "done";
    });
    if (depsmet) ready.push(node);
  }
  return ready;
};

const allNodesDone = (state: Record<string, unknown>): boolean => {
  const nodes = state.nodes as Map<string, DagNode>;
  for (const node of nodes.values()) {
    if (node.status !== "done" && node.status !== "failed") return false;
  }
  return true;
};

const buildNodePrompt = (node: DagNode, ctx: WorkflowContext): string => {
  const nodes = ctx.state.nodes as Map<string, DagNode>;
  const parts = [`# Task: ${node.id}\n\n${node.description}`];

  const depOutputs = node.dependencies
    .map(depId => nodes.get(depId))
    .filter((dep): dep is DagNode => dep != null && dep.output != null && dep.output.length > 0)
    .map(dep => `### ${dep.id} (${dep.agentType})\n\n${dep.output.slice(0, 2000)}`);

  if (depOutputs.length > 0) {
    parts.push(`\n## Context from Previous Steps\n\n${depOutputs.join("\n\n")}`);
  }

  const nodeDir = resolve(ctx.workDir, node.workDir);
  parts.push(`\n## Working Directory\n\nWork in: \`${nodeDir}\``);

  try {
    const gitLog = execSync("git log --oneline -10", { cwd: nodeDir, encoding: "utf-8" }).trim();
    if (gitLog) parts.push(`\n## Recent Commits (match this style)\n\n\`\`\`\n${gitLog}\n\`\`\``);
  } catch {}

  if (node.attempts > 0 && node.output) {
    parts.push(`\n## Previous Attempt Failed (attempt ${node.attempts})\n\n${node.output.slice(0, 1500)}`);
  }

  return parts.join("\n");
};

const STATUS_ICONS: Record<string, string> = {
  pending: "·", running: "⟳", done: "✓", failed: "✗",
};

let widgetRefreshInterval: ReturnType<typeof setInterval> | null = null;
let widgetCtx: WorkflowContext | null = null;

const startWidgetRefresh = (): void => {
  if (widgetRefreshInterval) return;
  widgetRefreshInterval = setInterval(() => {
    if (!widgetCtx) return;
    const nodes = widgetCtx.state.nodes as Map<string, DagNode>;
    const hasRunning = [...nodes.values()].some(n => n.status === "running");
    if (hasRunning) {
      updateWidget(widgetCtx);
    } else {
      stopWidgetRefresh();
    }
  }, 1000);
};

const stopWidgetRefresh = (): void => {
  if (widgetRefreshInterval) {
    clearInterval(widgetRefreshInterval);
    widgetRefreshInterval = null;
  }
};

const formatElapsed = (ms: number): string => {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${s % 60}s`;
  return `${Math.floor(m / 60)}h${m % 60}m`;
};

const formatProgress = (nodes: Map<string, DagNode>): string => {
  const now = Date.now();
  if (nodes.size === 0) return "No nodes active.";

  const lines: string[] = [];
  for (const n of nodes.values()) {
    const icon = STATUS_ICONS[n.status] || "?";
    const attempt = n.attempts > 1 ? ` (attempt ${n.attempts})` : "";
    let time = "";
    if (n.status === "running" && n.startedAt) time = ` ${formatElapsed(now - n.startedAt)}`;
    else if (n.finishedAt && n.startedAt) time = ` ${formatElapsed(n.finishedAt - n.startedAt)}`;
    const pid = n.processId ? ` (${n.processId})` : "";
    lines.push(`${icon} ${n.id}${pid} [${n.agentType}]: ${n.description.slice(0, 50)}${attempt}${time} [${n.status}]`);
  }

  const done = [...nodes.values()].filter(n => n.status === "done").length;
  const running = [...nodes.values()].filter(n => n.status === "running").length;
  const failed = [...nodes.values()].filter(n => n.status === "failed").length;
  const starts = [...nodes.values()].filter(n => n.startedAt).map(n => n.startedAt!);
  const firstStart = Math.min(...starts);
  const elapsed = isFinite(firstStart) ? ` | elapsed ${formatElapsed(now - firstStart)}` : "";
  lines.push("", `${done}/${nodes.size} done, ${running} running, ${failed} failed${elapsed}`);
  return lines.join("\n");
};

const WIDGET_FADE_MS = 10_000;

const updateWidget = (ctx: WorkflowContext): void => {
  widgetCtx = ctx;
  const nodes = ctx.state.nodes as Map<string, DagNode>;
  const now = Date.now();

  const isVisible = (status: string, finishedAt?: number) =>
    status === "running" || status === "pending" || (finishedAt && now - finishedAt < WIDGET_FADE_MS);

  const visibleNodes = [...nodes.values()].filter(n => isVisible(n.status, n.finishedAt));

  if (visibleNodes.length === 0) {
    ctx.setWidget("dag-progress", undefined);
    stopWidgetRefresh();
    return;
  }

  const hasRunning = visibleNodes.some(n => n.status === "running");
  const done = [...nodes.values()].filter(n => n.status === "done").length;
  const failed = [...nodes.values()].filter(n => n.status === "failed").length;
  const starts = [...nodes.values()].filter(n => n.startedAt).map(n => n.startedAt!);
  const firstStart = Math.min(...starts);
  const elapsed = isFinite(firstStart) ? ` | ${formatElapsed(now - firstStart)}` : "";
  const failLabel = failed > 0 ? `, ${failed} failed` : "";
  const lines: string[] = [`─── ${done}/${nodes.size} done${failLabel}${elapsed} ───`];

  for (const n of visibleNodes) {
    const icon = STATUS_ICONS[n.status] || "?";
    const attempt = n.attempts > 1 ? ` #${n.attempts}` : "";
    let time = "";
    if (n.status === "running" && n.startedAt) time = ` ${formatElapsed(now - n.startedAt)}`;
    else if (n.finishedAt && n.startedAt) time = ` ${formatElapsed(n.finishedAt - n.startedAt)}`;
    const pid = n.processId ? ` (${n.processId})` : "";
    lines.push(` ${icon} ${n.id}${pid} [${n.agentType}]${attempt}${time} [${n.status}]`);
    if (n.processId && n.status === "running") {
      const taskInfo = getTask(n.processId);
      if (taskInfo?.lastToolCall) {
        lines.push(`   └─ ${taskInfo.lastToolCall.slice(0, 60)}`);
      }
    }
  }
  ctx.setWidget("dag-progress", lines);
  if (hasRunning) startWidgetRefresh();
  else stopWidgetRefresh();
};


// ── Validation Commands Discovery ──────────────────────────────────────

type ValidationConfig = { dir: string; commands: string[] };

const FRONTMATTER_RE = /^---\n([\s\S]*?)\n---/;

const parseValidationCommands = (content: string): string[] => {
  const match = content.match(FRONTMATTER_RE);
  if (!match) return [];
  const yaml = match[1];
  const commandsMatch = yaml.match(/validation_commands:\s*\n((?:\s+-\s+.+\n?)*)/);
  if (!commandsMatch) return [];
  return commandsMatch[1]
    .split("\n")
    .map(line => line.replace(/^\s+-\s+/, "").trim())
    .filter(Boolean);
};

const findValidationConfigs = (startDir: string): ValidationConfig[] => {
  const configs: ValidationConfig[] = [];
  const visited = new Set<string>();

  const walk = (dir: string): void => {
    if (visited.has(dir)) return;
    visited.add(dir);
    try {
      const entries = readdirSync(dir, { withFileTypes: true });
      for (const entry of entries) {
        if (entry.name === "node_modules" || entry.name === ".git" || entry.name === ".pi") continue;
        if (entry.name === "AGENTS.md" && entry.isFile()) {
          const content = readFileSync(join(dir, "AGENTS.md"), "utf-8");
          const commands = parseValidationCommands(content);
          if (commands.length > 0) configs.push({ dir, commands });
        }
        if (entry.isDirectory()) walk(join(dir, entry.name));
      }
    } catch {}
  };

  walk(startDir);
  return configs;
};


// ── Effect Constructors ─────────────────────────────────────────────────

const ScheduleReady = (): Effect => ({
  type: "schedule-ready",
  priority: 10,
  handle: async (ctx) => {
    const ready = getReadyNodes(ctx.state);
    if (ready.length === 0) {
      if (allNodesDone(ctx.state)) return [RunValidation()];
      return [];
    }
    return ready.map(node => RunNode(node));
  },
});

const RunValidation = (): Effect => ({
  type: "run-validation",
  priority: 100,
  handle: async (ctx) => {
    const configs = findValidationConfigs(ctx.workDir);

    if (configs.length > 0) {
      const failures: string[] = [];
      for (const cfg of configs) {
        for (const cmd of cfg.commands) {
          try {
            execSync(cmd, { cwd: cfg.dir, encoding: "utf-8", stdio: ["pipe", "pipe", "pipe"], timeout: 60_000 });
          } catch (e: unknown) {
            const err = e as { stderr?: string; stdout?: string };
            failures.push(`\`${cmd}\` (in ${cfg.dir}):\n${(err.stderr || err.stdout || "unknown error").slice(0, 500)}`);
          }
        }
      }

      if (failures.length > 0) {
        ctx.sendMessage(
          `## Validation Failed\n\n${failures.join("\n\n")}\n\nFix validation issues before finishing.`,
          { display: true, triggerTurn: true },
        );
        return [];
      }

      ctx.sendMessage("Validation commands passed.", { display: true });
    }

    const nodes = ctx.state.nodes as Map<string, DagNode>;
    const hadEscalations = [...nodes.values()].some(n => n.status === "failed");
    const hint = nodes.size >= 3 || hadEscalations
      ? " Consider running `reflect` to capture learnings worth documenting."
      : "";
    ctx.sendMessage(`All nodes complete.${hint}`, { display: true });
    return [];
  },
});

const RunNode = (node: DagNode): Effect => ({
  type: "run-node",
  priority: 50,
  handle: async (ctx) => {
    if (node.status === "failed") return [];
    node.status = "running";
    node.attempts += 1;
    node.startedAt = node.startedAt ?? Date.now();
    updateWidget(ctx);

    const nodeDir = resolve(ctx.workDir, node.workDir);
    const agent = agentConfigs[node.agentType] ?? builder;
    const prompt = buildNodePrompt(node, ctx);

    const spawnOpts = {
      cwd: nodeDir,
      onSpawn: (id: string) => { node.processId = id; updateWidget(ctx); },
    };

    let result: { ok: boolean; output: string; errors: string; exitCode: number };
    try {
      result = await ctx.spawn(agent, prompt, spawnOpts);
    } catch (err) {
      result = { ok: false, output: "", errors: String(err), exitCode: -1 };
    }

    node.output = result.output || result.errors || "";

    if (result.ok) {
      node.status = "done";
      node.finishedAt = Date.now();
      updateWidget(ctx);

      // Notify orchestrator — triggerTurn only when no more work is pending
      // (intermediate pipeline nodes stay quiet, terminal/explorer/critic nodes trigger)
      const nodes = ctx.state.nodes as Map<string, DagNode>;
      const hasMore = [...nodes.values()].some(n => n.status === "running" || n.status === "pending");
      ctx.sendMessage(
        `## Node Complete: ${node.id} [${node.agentType}]\n\n${(node.output || "(no output)").slice(0, 3000)}`,
        { display: true, triggerTurn: !hasMore },
      );

      return [ScheduleReady()];
    }

    // Retry
    if (node.attempts < node.maxAttempts) {
      node.status = "pending";
      return [RunNode(node)];
    }

    const errorDetail = node.output || "(no output)";
    return [Escalate(node.id, `Failed after ${node.attempts} attempt(s).\n\nLast output:\n${errorDetail.slice(0, 1000)}`)];
  },
});

const cascadeFail = (nodes: Map<string, DagNode>, failedId: string): string[] => {
  const cancelled: string[] = [];
  const failed = new Set([failedId]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const n of nodes.values()) {
      if (n.status === "pending" && !failed.has(n.id) && n.dependencies.some(d => failed.has(d))) {
        n.status = "failed";
        n.finishedAt = Date.now();
        n.summary = `Cancelled — depends on failed node "${failedId}"`;
        failed.add(n.id);
        cancelled.push(n.id);
        changed = true;
      }
    }
  }
  return cancelled;
};

const Escalate = (nodeId: string, reason: string): Effect => ({
  type: "escalate",
  priority: 100,
  handle: async (ctx) => {
    const nodes = ctx.state.nodes as Map<string, DagNode>;
    const node = nodes.get(nodeId);
    if (node) { node.status = "failed"; node.finishedAt = Date.now(); }
    const cancelled = cascadeFail(nodes, nodeId);
    updateWidget(ctx);

    const cascadeNote = cancelled.length > 0
      ? `\n\nCascade-cancelled ${cancelled.length} dependent node(s): ${cancelled.join(", ")}`
      : "";
    ctx.sendMessage(
      `## Escalation: ${nodeId}\n\n${reason}${cascadeNote}\n\nDecide: retry with different instructions, skip this node, or revise the plan.`,
      { display: true, triggerTurn: true },
    );
    return [];
  },
});

// ── Disguise Export ─────────────────────────────────────────────────────

export default defineDisguise({
  lead: {
    model: config.models.orchestrator,
    context: ["prompts/project-overview.md", "prompts/lead.md"],

    tools: {
      "read-file": {
        description: "Read a file the user provided via @path that was truncated. Only for completing truncated @file references.",
        params: {
          path: { type: "string", description: "File path relative to repo root (must be a file the user referenced)" },
          offset: { type: "string", description: "Start line (1-based, default: 1)", required: false },
          limit: { type: "string", description: "Max lines to read (default: 500)", required: false },
        },
        execute: async (params, ctx) => {
          const target = resolve(ctx.workDir, params.path as string);
          try {
            const content = readFileSync(target, "utf-8");
            const lines = content.split("\n");
            const offset = Math.max(0, parseInt(params.offset as string || "1", 10) - 1);
            const limit = parseInt(params.limit as string || "500", 10);
            const slice = lines.slice(offset, offset + limit);
            const numbered = slice.map((line, i) => `${offset + i + 1}\t${line}`).join("\n");
            const truncated = lines.length > offset + limit
              ? `\n\n... (${lines.length - offset - limit} more lines)`
              : "";
            return numbered + truncated;
          } catch (e: unknown) {
            return `Error: ${(e as Error).message}`;
          }
        },
      },
      "write-review": {
        description: "Write an implementation plan and submit it for user review. BLOCKING: writes the file, opens review UI, and waits for user to submit. The return value is the user's review result (approval or revision comments). Do NOT report that the review is starting — when this tool returns, the review is already complete.",
        longRunning: true,
        params: {
          content: { type: "string", description: "The full plan content in markdown format" },
        },
        execute: async (params, ctx) => {
          const content = params.content as string;
          const timestamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
          const planDir = resolve(ctx.workDir, "tmp/plans");
          const planPath = resolve(planDir, `plan-${timestamp}.md`);

          (ctx.state as Record<string, unknown>).planApproved = false;
          mkdirSync(planDir, { recursive: true });
          writeFileSync(planPath, content, "utf-8");

          // Run on_plan_ready hooks -- external tools decide the review flow
          const hooksConfig = loadHooksConfig(ctx.workDir);
          const hookResult = await runPlanHooks(hooksConfig, { plan_path: planPath, content }, ctx.workDir);

          if (hookResult) {
            if (hookResult.status === "approved") {
              (ctx.state as Record<string, unknown>).planApproved = true;
              return "Plan approved by user. Submit nodes via bg-plan.";
            }
            return `User requested changes to the plan:\n\n${hookResult.comments ?? ""}`;
          }

          // No hooks configured or all abstained -- inline review flow
          ctx.sendMessage(`## Plan for Review\n\n${content}`, { display: true });
          const choice = await ctx.select("Review plan", ["Approve", "Request changes"]);
          if (choice === "Approve") {
            (ctx.state as Record<string, unknown>).planApproved = true;
            return "Plan approved by user. Submit nodes via bg-plan.";
          }
          const feedback = await ctx.input("What changes are needed?", { timeout: 300_000, defaultValue: "" });
          return `Plan needs revision:\n\n${feedback}`;
        },
      },
      "progress": {
        description: "Show current DAG progress — nodes, statuses, and overall completion. Do NOT poll — results are pushed when nodes complete.",
        params: {},
        execute: async (_params, ctx) => {
          const now = Date.now();
          const last = (ctx.state as Record<string, unknown>).lastProgressCall as number | undefined;
          (ctx.state as Record<string, unknown>).lastProgressCall = now;

          if (last && now - last < 30_000) {
            return "Progress was shown recently. The widget shows live status. Continue chatting with the user — you'll be notified when nodes complete.";
          }

          const nodes = ctx.state.nodes as Map<string, DagNode>;
          return formatProgress(nodes);
        },
      },
      "terminate": {
        description: "Kill running subagent processes. Pass a node ID to kill a specific node, or 'all' to kill everything.",
        params: {
          id: { type: "string", description: "Node ID to kill, or 'all' to kill everything", required: false },
          reason: { type: "string", description: "Why execution is being terminated" },
        },
        execute: async (params, ctx) => {
          const nodes = ctx.state.nodes as Map<string, DagNode>;
          const id = params.id as string | undefined;
          const now = Date.now();

          if (!id || id === "all") {
            const runningNodes = [...nodes.values()].filter(n => n.status === "running" || n.status === "pending");
            killAllTasks();
            for (const node of runningNodes) {
              node.status = "failed";
              node.finishedAt = node.finishedAt ?? now;
            }
            updateWidget(ctx);
            return `Terminated ${runningNodes.length} node(s): ${params.reason}`;
          }

          const node = nodes.get(id);
          if (!node) return `"${id}" not found in nodes.`;
          if (node.status !== "running" && node.status !== "pending") return `Node "${id}" is already ${node.status}.`;

          if (node.processId) killTask(node.processId);
          node.status = "failed";
          node.finishedAt = node.finishedAt ?? now;
          updateWidget(ctx);
          return `Terminated node "${id}": ${params.reason}`;
        },
      },
      "bg-plan": {
        description: "Submit DAG nodes for execution. Each node spawns one agent. Nodes run when all their dependencies are done.",
        params: {
          nodes: {
            type: "array",
            description: "Array of node objects: { id, description, agentType, dependencies?: string[], workDir, maxAttempts? }. id = unique identifier. description = what the agent should do (becomes the prompt). agentType = which agent: 'builder' (writes code), 'reviewer' (reviews changes, read-only), 'tester' (runs tests), 'prompt-engineer' (writes prompts), 'explorer' (read-only codebase investigation), 'critic' (plan critique), 'reflector' (session reflection). dependencies = node IDs that must complete first (their output is injected as context). workDir = subdirectory relative to repo root. maxAttempts = retries on failure (default 3).",
          },
        },
        execute: async (params, ctx) => {
          const rawNodes = params.nodes as Array<{
            id: string;
            description: string;
            agentType?: string;
            dependencies?: string[];
            workDir: string;
            maxAttempts?: number;
          }>;

          // Read-only agents don't need plan approval
          const READ_ONLY_AGENTS = new Set(["explorer", "critic", "reflector", "reviewer"]);
          const hasWriteAgents = rawNodes.some(n => !READ_ONLY_AGENTS.has(n.agentType ?? "builder"));
          if (hasWriteAgents && !(ctx.state as Record<string, unknown>).planApproved) {
            return "Plan not yet approved. Use write-review to submit your plan for review first. (Read-only nodes like explorer/critic/reviewer don't need approval.)";
          }

          const nodes = ctx.state.nodes as Map<string, DagNode>;
          for (const raw of rawNodes) {
            nodes.set(raw.id, {
              id: raw.id,
              description: raw.description,
              agentType: raw.agentType ?? "builder",
              dependencies: raw.dependencies ?? [],
              workDir: raw.workDir ?? ".",
              status: "pending",
              attempts: 0,
              maxAttempts: raw.maxAttempts ?? 3,
            });
          }

          updateWidget(ctx);
          ctx.dispatch([ScheduleReady()]);
          return `${rawNodes.length} nodes submitted. Execution starting. Do NOT poll progress — you will be notified when nodes complete. Continue chatting with the user.`;
        },
      },
    },

    state: () => ({
      nodes: new Map<string, DagNode>(),
      planApproved: false,
    }),

    dispatchOpts: { concurrency: 3 },
  },

  solo: {
    context: ["prompts/project-overview.md"],
  },
});
