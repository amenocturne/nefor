import type {
  RuntimeHost, AgentConfig, AgentResult, SpawnOpts, SkillResult,
  BackgroundSkillHandle, SkillRegistry, ToolRegistration, MessageOpts, InputOpts,
  ToolCallEvent, ToolCallResponse,
} from "./types.ts";
import { discoverSkills, runSkill, runSkillBackground } from "./skills.ts";
import { spawnAgent, generateTaskId } from "../task-manager.ts";
import { spawn } from "child_process";
import { existsSync, readFileSync, readdirSync } from "fs";
import { join, resolve } from "path";

// ── Types ──────────────────────────────────────────────────────────────

export type PiHost = RuntimeHost & {
  getSystemPromptAppendix(): string;
  resolveInput(response: string): void;
  getToolCallHandler(): ((event: ToolCallEvent) => Promise<ToolCallResponse>) | null;
  getSkillRegistry(): SkillRegistry;
  getToolRegistry(): Map<string, ToolRegistration>;
};

// ── Helpers ────────────────────────────────────────────────────────────

const SKIP_EXTENSIONS = new Set(["background-tasks", "agent-teams", "disguise"]);

const getExtensionsForSubagent = (piDir: string): string[] => {
  const extDir = join(piDir, "extensions");
  if (!existsSync(extDir)) return [];
  const paths: string[] = [];
  for (const entry of readdirSync(extDir)) {
    if (SKIP_EXTENSIONS.has(entry)) continue;
    const full = resolve(extDir, entry);
    if (existsSync(join(full, "index.ts")) || existsSync(join(full, "package.json"))) {
      paths.push(full);
    }
  }
  return paths;
};

const waitForTask = (info: { status: string; output: string }, timeoutMs = 600_000): Promise<typeof info> =>
  new Promise((res) => {
    const start = Date.now();
    const check = setInterval(() => {
      if (info.status !== "running" && info.status !== "waiting") {
        clearInterval(check);
        res(info);
      } else if (Date.now() - start > timeoutMs) {
        clearInterval(check);
        (info as any).status = "failed";
        info.output += "\n[timed out]";
        res(info);
      }
    }, 500);
  });

// ── Factory ────────────────────────────────────────────────────────────

export function createPiHost(pi: any, piDirPath: string, extCtx?: any): PiHost {
  let systemPromptAppendix = "";
  let skillRegistry: SkillRegistry = new Map();
  const toolRegistry = new Map<string, ToolRegistration>();
  let toolCallHandler: ((event: ToolCallEvent) => Promise<ToolCallResponse>) | null = null;

  return {
    cwd: process.cwd(),
    piDir: piDirPath,

    // ── Tool management ──────────────────────────────────────────────

    registerTool(spec) {
      toolRegistry.set(spec.name, spec);
    },

    removeTool(name) {
      toolRegistry.delete(name);
      const active = pi.getActiveTools().filter((t: string) => t !== name);
      pi.setActiveTools(active);
    },

    setActiveTools(tools) { pi.setActiveTools(tools); },
    getActiveTools() { return pi.getActiveTools(); },

    // ── Messaging ────────────────────────────────────────────────────

    sendMessage(content, opts) {
      const piOpts: Record<string, unknown> = {};
      if (opts?.triggerTurn) piOpts.triggerTurn = true;
      // Non-displayed messages are system directives — deliver as steer
      // so the model treats them as instructions, not user chat
      if (!opts?.display) piOpts.deliverAs = "steer";

      pi.sendMessage(
        { customType: "workflow", content, display: opts?.display ?? false },
        Object.keys(piOpts).length > 0 ? piOpts : undefined,
      );
    },

    appendSystemPrompt(content) {
      systemPromptAppendix += (systemPromptAppendix ? "\n\n" : "") + content;
    },

    // ── Agent spawning ───────────────────────────────────────────────

    async spawnAgent(config, prompt, opts) {
      let systemPrompt = "";
      if (config.prompt) {
        const promptPath = resolve(piDirPath, config.prompt);
        if (existsSync(promptPath)) {
          systemPrompt = readFileSync(promptPath, "utf-8");
        }
      }

      const fullPrompt = systemPrompt
        ? `${systemPrompt}\n\n# Task\n\n${prompt}`
        : prompt;

      const model = config.model ?? "";
      const provider = config.provider;
      const extensions = opts?.extensions ?? getExtensionsForSubagent(piDirPath);

      const agentCwd = opts?.cwd ?? process.cwd();

      const id = generateTaskId();
      opts?.onSpawn?.(id);
      const info = await spawnAgent(id, fullPrompt, model, provider, agentCwd, extensions, "silent");
      const completed = await waitForTask(info);

      return {
        ok: (completed as any).status === "done",
        output: (completed as any).finalReport || completed.output || "",
        errors: (completed as any).errors || "",
        filesChanged: [],
        exitCode: (completed as any).exitCode ?? 1,
      };
    },

    // ── Skills ───────────────────────────────────────────────────────

    discoverSkills(skillsDir) {
      skillRegistry = discoverSkills(skillsDir);
      return skillRegistry;
    },

    async runSkill(name, args) {
      const entry = skillRegistry.get(name);
      if (!entry) return { stdout: `skill "${name}" not found`, exitCode: 1 };
      return runSkill(entry, args);
    },

    runSkillBackground(name, args) {
      const entry = skillRegistry.get(name);
      if (!entry) return null;
      return runSkillBackground(entry, args);
    },

    openUrl(url) {
      const proc = spawn("open", [url], { stdio: "ignore", detached: true });
      proc.unref();
    },

    setWidget(key, lines) {
      if (extCtx?.ui?.setWidget) {
        extCtx.ui.setWidget(key, lines ?? undefined);
      }
    },

    // ── Tool-call interception ───────────────────────────────────────

    onToolCall(handler) {
      toolCallHandler = handler;
    },

    // ── Structured input ─────────────────────────────────────────────
    // Uses Pi's native UI prompts which actually stop the agent and
    // wait for user interaction (not message-based which the model
    // would respond to itself).

    async input(prompt, opts) {
      const maxRetries = opts?.maxRetries ?? 3;

      for (let attempt = 0; attempt <= maxRetries; attempt++) {
        const displayPrompt = attempt === 0 ? prompt : `Invalid format. ${prompt}`;

        const raw = await pi.ui.input("Workflow", displayPrompt, {
          timeout: opts?.timeout,
        });

        // ui.input returns undefined if cancelled/escaped
        if (raw === undefined) {
          if (opts?.defaultValue !== undefined) return opts.defaultValue;
          continue;
        }

        if (!opts?.validate) return raw;

        const cleaned = opts.validate(raw);
        if (cleaned !== null) return cleaned;
      }

      if (opts?.defaultValue !== undefined) return opts.defaultValue;
      throw new Error("input validation failed after max retries");
    },

    async select(title, options) {
      return pi.ui.select(title, options);
    },

    // ── Extension hooks ──────────────────────────────────────────────

    getSystemPromptAppendix() { return systemPromptAppendix; },

    resolveInput(_response) {
      // No-op — input is now handled via Pi's native ui.input()
    },

    getToolCallHandler() { return toolCallHandler; },
    getSkillRegistry() { return skillRegistry; },
    getToolRegistry() { return toolRegistry; },
  };
}
