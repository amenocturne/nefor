/**
 * Disguise — Pi Extension (workflow framework edition)
 *
 * Loads `.pi/disguise.ts` which exports named disguise configs. Each disguise
 * defines context injection, custom tools, write-path restrictions, and write
 * hooks. The user switches disguises via /disguise.
 */

import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { Text } from "@mariozechner/pi-tui";
import { Type } from "@sinclair/typebox";
import { existsSync, readFileSync } from "fs";
import { killTask, killAllTasks } from "../../lib/task-manager.ts";
import { setSegment } from "../statusline/index.ts";
import { join, resolve } from "path";
import { parse } from "yaml";
import type {
  DisguiseExport,
  DisguiseConfig,
  Runtime,
  ToolDef,
  WorkflowContext,
} from "../../lib/workflow/types.ts";
import type { PiHost } from "../../lib/workflow/host.ts";

// Lazy-loaded to avoid top-level yaml import from skills.ts
let _createRuntime: typeof import("../../lib/workflow/runtime.ts").createRuntime;
let _createPiHost: typeof import("../../lib/workflow/host.ts").createPiHost;

async function loadWorkflowFramework(): Promise<boolean> {
  if (_createRuntime) return true;
  try {
    const runtime = await import("../../lib/workflow/runtime.ts");
    const host = await import("../../lib/workflow/host.ts");
    _createRuntime = runtime.createRuntime;
    _createPiHost = host.createPiHost;
    return true;
  } catch (err) {
    console.error(`Failed to load workflow framework: ${err}`);
    return false;
  }
}

// ── State ───────────────────────────────────────────────────────────────

let runtime: Runtime | null = null;
let host: PiHost | null = null;
let disguiseExport: DisguiseExport | null = null;
let activeDisguiseName: string | null = null;
let activeConfig: DisguiseConfig | null = null;
let workflowCtx: WorkflowContext | null = null;
let piDir = "";
let originalTools: string[] = [];
let workspaceContext = "";
// Track active session to detect /new resets
let lastSessionReason = "";

// ── Glob Matching ───────────────────────────────────────────────────────

function globMatch(pattern: string, text: string): boolean {
  const escaped = pattern
    .replace(/([.+^${}()|[\]\\])/g, "\\$1")
    .replace(/\*\*/g, "<<<GLOBSTAR>>>")
    .replace(/\*/g, "[^/]*")
    .replace(/<<<GLOBSTAR>>>/g, ".*")
    .replace(/\?/g, ".");
  return new RegExp("^" + escaped + "$").test(text);
}

// ── Disguise Loading ────────────────────────────────────────────────────

async function loadDisguiseTs(dir: string): Promise<DisguiseExport | null> {
  const path = join(dir, "disguise.ts");
  if (!existsSync(path)) return null;
  try {
    const mod = await import(path);
    return mod.default as DisguiseExport;
  } catch (err) {
    console.error(`Failed to load disguise.ts: ${err}`);
    return null;
  }
}

// ── Context Injection ───────────────────────────────────────────────────

function readContextFile(filePath: string): string | null {
  const resolved = resolve(piDir, filePath);
  if (!existsSync(resolved)) return null;
  try {
    return readFileSync(resolved, "utf-8");
  } catch {
    return null;
  }
}

function collectContextForPrompt(files: string[]): string {
  const parts: string[] = [];
  for (const file of files) {
    const content = readContextFile(file);
    if (content) parts.push(`## ${file}\n\n${content}`);
  }
  return parts.join("\n\n");
}

// ── WORKSPACE.yaml ──────────────────────────────────────────────────────

function loadWorkspaceContext(): string {
  let dir = process.cwd();
  for (let i = 0; i < 5; i++) {
    const candidate = join(dir, "WORKSPACE.yaml");
    if (existsSync(candidate)) {
      try {
        return readFileSync(candidate, "utf-8");
      } catch {
        return "";
      }
    }
    const parent = resolve(dir, "..");
    if (parent === dir) break;
    dir = parent;
  }
  return "";
}

// ── Tool Registration ───────────────────────────────────────────────────

function registerDisguiseTools(
  pi: ExtensionAPI,
  tools: Record<string, ToolDef>,
): void {
  for (const [name, def] of Object.entries(tools)) {
    const properties: Record<string, any> = {};
    const required: string[] = [];

    for (const [pName, pDef] of Object.entries(def.params)) {
      if (pDef.type === "string") {
        properties[pName] = Type.String({ description: pDef.description });
      } else if (pDef.type === "number") {
        properties[pName] = Type.Number({ description: pDef.description });
      } else if (pDef.type === "boolean") {
        properties[pName] = Type.Boolean({ description: pDef.description });
      } else if (pDef.type === "array") {
        properties[pName] = Type.Array(Type.Unknown(), {
          description: pDef.description,
        });
      } else {
        properties[pName] = Type.Unknown({ description: pDef.description });
      }
      if (pDef.required !== false) required.push(pName);
    }

    const schema = Type.Object(properties, { required });

    pi.registerTool({
      name,
      label: name,
      description: def.description,
      parameters: schema,

      async execute(_toolCallId, params, signal, _onUpdate, _ctx) {
        if (!workflowCtx) {
          return {
            content: [
              { type: "text" as const, text: "Workflow not active" },
            ],
          };
        }
        // Availability gate — check before execution
        if (def.available && !def.available(workflowCtx.state)) {
          const allTools = workflowCtx.config.tools ?? {};
          const available = Object.entries(allTools)
            .filter(([, t]) => !t.available || t.available(workflowCtx.state))
            .map(([n]) => n);
          const msg = def.unavailableMessage
            ?? `Tool "${name}" is not available right now. Available tools: ${available.join(", ")}`;
          return { content: [{ type: "text" as const, text: msg }] };
        }
        if (signal?.aborted && !def.longRunning) {
          return { content: [{ type: "text" as const, text: "Cancelled" }] };
        }
        try {
          // Long-running tools (e.g. write-review) ignore abort signal —
          // they block on user interaction and must run to completion
          const abortPromise = (!def.longRunning && signal)
            ? [new Promise<never>((_, reject) => {
                signal.addEventListener("abort", () => reject(new Error("Cancelled")), { once: true });
              })]
            : [];
          const result = await Promise.race([
            def.execute(params, workflowCtx),
            ...abortPromise,
          ]);
          const text =
            typeof result === "string" ? result : JSON.stringify(result);
          return { content: [{ type: "text" as const, text }] };
        } catch (err: any) {
          if (err.message === "Cancelled") {
            return { content: [{ type: "text" as const, text: "Cancelled" }] };
          }
          return {
            content: [
              { type: "text" as const, text: `Error: ${err.message}` },
            ],
          };
        }
      },

      renderCall(args, theme) {
        return new Text(
          theme.fg("toolTitle", theme.bold(`${name} `)) +
            theme.fg("dim", JSON.stringify(args).slice(0, 80)),
          0,
          0,
        );
      },

      renderResult(result, _options, theme) {
        const text =
          result.content[0]?.type === "text"
            ? (result.content[0] as any).text
            : "";
        const preview =
          text.length > 60 ? text.slice(0, 57) + "..." : text;
        return new Text(
          theme.fg("success", "-> ") + theme.fg("dim", preview),
          0,
          0,
        );
      },
    });
  }
}

// ── Disguise Activation ─────────────────────────────────────────────────

function activateDisguise(
  pi: ExtensionAPI,
  name: string,
  config: DisguiseConfig,
  ctx: ExtensionContext,
): void {
  activeDisguiseName = name;
  activeConfig = config;

  // Restore original tools before applying new restrictions
  if (originalTools.length > 0) {
    pi.setActiveTools(originalTools);
  }

  // Create runtime + host if not yet created
  if (!runtime) runtime = _createRuntime();
  if (!host) host = _createPiHost(pi, piDir, ctx);

  // Activate the disguise in the runtime
  workflowCtx = runtime.activate(config, host);

  // Register custom tools and restrict tool set for lead disguises
  if (config.tools && Object.keys(config.tools).length > 0) {
    registerDisguiseTools(pi, config.tools);

    const activeTools = [...Object.keys(config.tools), "read"];
    if (config.writePaths && config.writePaths.length > 0) {
      activeTools.push("write");
    }
    pi.setActiveTools(activeTools);
  }

  // Discover skills
  const skillsDir = join(piDir, "skills");
  if (existsSync(skillsDir)) {
    host.discoverSkills(skillsDir);
  }

  setSegment("disguise", name, { order: 20, color: "accent" });
}

function injectContextMessages(pi: ExtensionAPI, config: DisguiseConfig): void {
  if (workspaceContext) {
    pi.sendMessage({
      customType: "context-injection",
      content: `## WORKSPACE.yaml\n\n\`\`\`yaml\n${workspaceContext}\`\`\``,
      display: false,
    });
  }
  if (config.context) {
    for (const file of config.context) {
      const content = readContextFile(file);
      if (content) {
        pi.sendMessage({
          customType: "context-injection",
          content: `## ${file}\n\n${content}`,
          display: false,
        });
      }
    }
  }
}

function buildContextForPrompt(config: DisguiseConfig): string {
  const parts: string[] = [];
  if (workspaceContext) {
    parts.push(`## WORKSPACE.yaml\n\n\`\`\`yaml\n${workspaceContext}\`\`\``);
  }
  if (config.context) {
    parts.push(collectContextForPrompt(config.context));
  }
  return parts.filter(Boolean).join("\n\n");
}

// ── Extension Entry ─────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {

  // ── session_start ───────────────────────────────────────────────────

  pi.on("session_start", async (event, ctx) => {
    lastSessionReason = event.reason;
    piDir = join(process.cwd(), ".pi");
    workspaceContext = loadWorkspaceContext();

    // Reset all state on /new so the session starts clean
    if (event.reason === "new") {
      runtime = null;
      host = null;
      originalTools = [];
    }

    // Capture the default tool set before any disguise modifies it
    if (event.reason === "startup" || originalTools.length === 0) {
      originalTools = pi.getActiveTools();
    }

    // Load workflow framework + disguise.ts
    const frameworkOk = await loadWorkflowFramework();
    if (!frameworkOk) {
      setSegment("disguise", "error", { order: 20, color: "error" });
      return;
    }

    disguiseExport = await loadDisguiseTs(piDir);
    if (!disguiseExport || Object.keys(disguiseExport).length === 0) {
      setSegment("disguise", "none", { order: 20, color: "dim" });
      return;
    }

    // Only activate on first start or explicit /new — preserve runtime
    // state (like planApproved) across resume/compact events
    const names = Object.keys(disguiseExport);
    if (!runtime || event.reason === "new" || event.reason === "startup") {
      activateDisguise(pi, names[0], disguiseExport[names[0]], ctx);
    } else {
      setSegment("disguise", activeDisguiseName || "?", { order: 20, color: "accent" });
    }
  });

  // ── before_agent_start ──────────────────────────────────────────────

  pi.on("before_agent_start", async (event) => {
    const parts: string[] = [];

    if (activeDisguiseName && activeConfig) {
      parts.push(`## Active Disguise: ${activeDisguiseName}`);

      const contextContent = buildContextForPrompt(activeConfig);
      if (contextContent) parts.push(contextContent);
    }

    // System prompt from host (effects may have appended to it)
    if (host) {
      const appendix = host.getSystemPromptAppendix();
      if (appendix) parts.push(appendix);
    }

    if (parts.length === 0) return {};
    return { systemPrompt: event.systemPrompt + "\n\n" + parts.join("\n\n") };
  });

  // ── tool_call ───────────────────────────────────────────────────────

  pi.on("tool_call", async (event) => {
    if (!activeConfig) return { block: false };

    const toolName = event.toolName;
    const input = event.input as Record<string, unknown>;

    // Write path enforcement
    if (toolName === "write" || toolName === "edit") {
      const filePath = (input.file_path ?? input.path ?? "") as string;

      if (activeConfig.writePaths && activeConfig.writePaths.length > 0) {
        const allowed = activeConfig.writePaths.some((pattern) =>
          globMatch(pattern, filePath),
        );
        if (!allowed) {
          return {
            block: true,
            reason: `Write blocked: ${filePath} not in allowed paths: ${activeConfig.writePaths.join(", ")}`,
          };
        }
      }

      // Fire write hooks
      if (activeConfig.writeHooks) {
        for (const [pattern, hookFn] of Object.entries(
          activeConfig.writeHooks,
        )) {
          if (globMatch(pattern, filePath)) {
            const effects = hookFn(filePath);
            if (effects.length > 0 && runtime) {
              // Allow the write first, then dispatch effects
              setTimeout(() => runtime!.dispatch(effects), 0);
            }
          }
        }
      }
    }

    // Delegate to workflow's tool call handler if registered
    if (host) {
      const handler = host.getToolCallHandler();
      if (handler) {
        return handler({ toolName, input });
      }
    }

    return { block: false };
  });

  // ── session_compact ─────────────────────────────────────────────────

  pi.on("session_compact", async () => {
    pi.sendMessage(
      {
        customType: "compaction-resume",
        content:
          "Context was compacted. Resume from where you left off. " +
          "Do not recap, do not re-read files mentioned in the summary, " +
          "do not ask where we were. If the summary mentions pending work, do that next." +
          (activeDisguiseName
            ? ` Active disguise: ${activeDisguiseName}.`
            : ""),
        display: false,
      },
      { deliverAs: "steer" },
    );
  });

  // ── /approve command (user-only plan approval) ─────────────────────

  pi.registerCommand("approve", {
    description: "Approve the current plan so bg-plan can execute",
    handler: async (_args, ctx) => {
      if (!workflowCtx) {
        ctx.ui.notify("No active workflow.", "warning");
        return;
      }
      (workflowCtx.state as Record<string, unknown>).planApproved = true;
      pi.sendMessage(
        { customType: "workflow", content: "Plan approved by user. You may now submit tasks via bg-plan.", display: true },
        { deliverAs: "steer" },
      );
      ctx.ui.notify("Plan approved", "info");
    },
  });

  // ── /terminate command (kill all orchestrator agents) ───────────────

  pi.registerCommand("terminate", {
    description: "Kill running subagent processes and stop DAG execution",
    handler: async (_args, ctx) => {
      if (!workflowCtx) {
        ctx.ui.notify("No active workflow.", "warning");
        return;
      }

      const tasks = workflowCtx.state.tasks as Map<string, { id: string; status: string; finishedAt?: number; processId?: string }>;
      const active = tasks ? [...tasks.values()].filter(t => t.status === "running" || t.status === "pending") : [];

      if (active.length === 0) {
        ctx.ui.notify("No running or pending tasks.", "info");
        return;
      }

      const options = [
        ...active.map(t => {
          const pid = t.processId ? ` (${t.processId})` : "";
          return `${t.id}${pid} [${t.status}]`;
        }),
        "Kill all",
      ];

      const choice = await ctx.ui.select("Terminate task", options);
      if (choice === undefined) return;

      const now = Date.now();

      if (choice === "Kill all") {
        killAllTasks();
        for (const task of active) {
          task.status = "failed";
          task.finishedAt = task.finishedAt ?? now;
        }
        workflowCtx.setWidget("dag-progress", undefined);
        pi.sendMessage(
          { customType: "workflow", content: `Terminated ${active.length} task(s). All execution stopped.`, display: true },
          { deliverAs: "steer" },
        );
        ctx.ui.notify(`Terminated ${active.length} task(s)`, "warning");
      } else {
        const taskId = choice.split(" ")[0];
        const task = tasks.get(taskId);
        if (!task) { ctx.ui.notify(`Task "${taskId}" not found.`, "error"); return; }
        if (task.processId) killTask(task.processId);
        task.status = "failed";
        task.finishedAt = task.finishedAt ?? now;
        // Re-render widget with updated state
        const allTasks = [...tasks.values()];
        const hasRunning = allTasks.some(t => t.status === "running");
        if (!hasRunning) workflowCtx.setWidget("dag-progress", undefined);
        pi.sendMessage(
          { customType: "workflow", content: `Terminated task "${taskId}".`, display: true },
          { deliverAs: "steer" },
        );
        ctx.ui.notify(`Terminated task "${taskId}"`, "warning");
      }
    },
  });

  // ── /disguise command ───────────────────────────────────────────────

  pi.registerCommand("disguise", {
    description: "Switch between configured disguises",
    handler: async (_args, ctx) => {
      // Reload framework + disguise.ts
      if (!(await loadWorkflowFramework())) {
        ctx.ui.notify("Workflow framework failed to load.", "error");
        return;
      }
      disguiseExport = await loadDisguiseTs(piDir);
      if (!disguiseExport || Object.keys(disguiseExport).length === 0) {
        ctx.ui.notify(
          "No disguise.ts found or no disguises defined.",
          "warning",
        );
        return;
      }

      const names = Object.keys(disguiseExport);
      const options = names.map((name) => {
        const parts: string[] = [name];
        const cfg = disguiseExport![name];
        if (cfg.model) parts.push(`model:${cfg.model}`);
        if (cfg.tools) parts.push(`${Object.keys(cfg.tools).length} tools`);
        if (name === activeDisguiseName) parts.push("(active)");
        return parts.join(" | ");
      });

      const choice = await ctx.ui.select("Select disguise", options);
      if (choice === undefined) return;

      const chosenName = choice.split(" | ")[0];
      const chosenConfig = disguiseExport[chosenName];
      if (!chosenConfig) {
        ctx.ui.notify(`Disguise "${chosenName}" not found.`, "error");
        return;
      }

      // Reset runtime for new disguise
      runtime = null;
      host = null;

      activateDisguise(pi, chosenName, chosenConfig, ctx);

      // Mid-session: inject context via messages since before_agent_start already ran
      injectContextMessages(pi, chosenConfig);

      pi.sendMessage({
        customType: "disguise-switch",
        content: `Disguise switched to "${activeDisguiseName}".`,
        display: true,
      });

      ctx.ui.notify(`Switched to disguise: ${chosenName}`, "info");
    },
  });
}
