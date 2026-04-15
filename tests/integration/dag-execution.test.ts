/**
 * Integration tests for the nefor DAG execution pipeline.
 *
 * Tests the real dispatch loop with real concurrency and the exact DAG
 * patterns from nefor (Build -> Review -> Test -> Done -> ScheduleReady),
 * without importing disguise.ts (avoids installed-layout path issues).
 */
import { describe, it, expect, beforeEach } from "vitest";
import { dispatch } from "../../lib/workflow/runtime.ts";
import type { Effect, WorkflowContext } from "../../lib/workflow/types.ts";
import {
  createMockHost,
  agentResult,
  type MockRuntimeHost,
} from "../utils/mock-context.ts";
import { createRuntime } from "../../lib/workflow/runtime.ts";

// ── Task state (mirrors nefor/disguise.ts) ─────────────────────────────

type TaskState = {
  id: string;
  description: string;
  dependencies: string[];
  testCommand?: string;
  status: "pending" | "running" | "done" | "failed";
  attempts: number;
  summary?: string;
  startedAt?: number;
  finishedAt?: number;
};

// ── Helpers (mirrors nefor logic) ──────────────────────────────────────

const getReadyTasks = (state: Record<string, unknown>): TaskState[] => {
  const tasks = state.tasks as Map<string, TaskState>;
  const ready: TaskState[] = [];
  for (const task of tasks.values()) {
    if (task.status !== "pending") continue;
    const depsmet = task.dependencies.every((dep) => {
      const depTask = tasks.get(dep);
      return depTask && depTask.status === "done";
    });
    if (depsmet) ready.push(task);
  }
  return ready;
};

const allTasksDone = (state: Record<string, unknown>): boolean => {
  const tasks = state.tasks as Map<string, TaskState>;
  for (const task of tasks.values()) {
    if (task.status !== "done" && task.status !== "failed") return false;
  }
  return true;
};

const parseVerdict = (output: string): "PASS" | "CHANGES_NEEDED" | "FAIL" => {
  if (/VERDICT:\s*PASS/i.test(output)) return "PASS";
  if (/VERDICT:\s*CHANGES_NEEDED/i.test(output)) return "CHANGES_NEEDED";
  if (/VERDICT:\s*FAIL/i.test(output)) return "FAIL";
  return "PASS";
};

const cascadeFail = (
  tasks: Map<string, TaskState>,
  failedId: string,
): string[] => {
  const cancelled: string[] = [];
  const failed = new Set([failedId]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const t of tasks.values()) {
      if (
        t.status === "pending" &&
        !failed.has(t.id) &&
        t.dependencies.some((d) => failed.has(d))
      ) {
        t.status = "failed";
        t.finishedAt = Date.now();
        t.summary = `Cancelled -- depends on failed task "${failedId}"`;
        failed.add(t.id);
        cancelled.push(t.id);
        changed = true;
      }
    }
  }
  return cancelled;
};

// ── Agent configs ──────────────────────────────────────────────────────

const builderAgent = { model: "test-builder", prompt: "prompts/builder.md" };
const reviewerAgent = { model: "test-reviewer", prompt: "prompts/reviewer.md" };
const testerAgent = { model: "test-tester", prompt: "prompts/tester.md" };
const explorerAgent = {
  model: "test-explorer",
  prompt: "prompts/explorer.md",
  tools: { allow: ["read", "grep", "find", "ls", "glob"] },
};

// ── Effect constructors ────────────────────────────────────────────────

const ScheduleReady = (): Effect => ({
  type: "schedule-ready",
  handle: async (ctx) => {
    const ready = getReadyTasks(ctx.state);
    if (ready.length === 0) return [];
    return ready.map((t) => Build(t));
  },
});

const Build = (task: TaskState, attempt = 1, feedback?: string): Effect => ({
  type: "build",
  handle: async (ctx) => {
    if (task.status === "failed") return [];
    task.status = "running";
    task.attempts = attempt;
    task.startedAt = task.startedAt ?? Date.now();

    const prompt =
      `Build ${task.id}: ${task.description}` +
      (feedback ? `\n\nPrevious feedback:\n${feedback}` : "");
    let result: { ok: boolean; output: string };
    try {
      result = await ctx.spawn(builderAgent, prompt);
    } catch (err) {
      result = { ok: false, output: String(err) };
    }

    if (result.ok) return [ReviewCode(task, result.output)];
    if (attempt < 3) return [Build(task, attempt + 1, result.output)];
    return [
      Escalate(task.id, `Build failed after ${attempt} attempts`),
    ];
  },
});

const ReviewCode = (task: TaskState, buildOutput: string): Effect => ({
  type: "review-code",
  handle: async (ctx) => {
    if (task.status === "failed") return [];
    const prompt = `Review changes for "${task.id}": ${task.description}\n\nBuilder output:\n${buildOutput.slice(0, 2000)}`;
    let result: { ok: boolean; output: string };
    try {
      result = await ctx.spawn(reviewerAgent, prompt);
    } catch (err) {
      result = { ok: false, output: String(err) };
    }
    const verdict = parseVerdict(result.output);

    if (verdict === "PASS") return [TestCode(task)];
    if (verdict === "CHANGES_NEEDED" && task.attempts < 3) {
      return [Build(task, task.attempts + 1, result.output)];
    }
    return [
      Escalate(
        task.id,
        `Review failed after ${task.attempts} attempts`,
      ),
    ];
  },
});

const TestCode = (task: TaskState): Effect => ({
  type: "test-code",
  handle: async (ctx) => {
    if (task.status === "failed") return [];
    if (!task.testCommand) return [TaskDone(task)];

    let result: { ok: boolean; output: string };
    try {
      result = await ctx.spawn(testerAgent, `Run: ${task.testCommand}`);
    } catch (err) {
      result = { ok: false, output: String(err) };
    }
    const verdict = parseVerdict(result.output);

    if (verdict === "PASS") return [TaskDone(task)];
    if (task.attempts < 3) {
      return [Build(task, task.attempts + 1, result.output)];
    }
    return [
      Escalate(
        task.id,
        `Tests failed after ${task.attempts} attempts`,
      ),
    ];
  },
});

const TaskDone = (task: TaskState): Effect => ({
  type: "task-done",
  handle: async (_ctx) => {
    task.status = "done";
    task.finishedAt = Date.now();
    task.summary = `Task ${task.id} completed successfully.`;
    return [ScheduleReady()];
  },
});

const Escalate = (taskId: string, reason: string): Effect => ({
  type: "escalate",
  handle: async (ctx) => {
    const tasks = ctx.state.tasks as Map<string, TaskState>;
    const task = tasks.get(taskId);
    if (task) {
      task.status = "failed";
      task.finishedAt = Date.now();
    }
    const cancelled = cascadeFail(tasks, taskId);

    const cascadeNote =
      cancelled.length > 0
        ? `\n\nCascade-cancelled ${cancelled.length} dependent task(s): ${cancelled.join(", ")}`
        : "";
    ctx.sendMessage(
      `## Escalation: ${taskId}\n\n${reason}${cascadeNote}`,
      { display: true, triggerTurn: true },
    );
    return [];
  },
});

// ── Test helpers ───────────────────────────────────────────────────────

const makeTask = (
  id: string,
  deps: string[] = [],
  opts?: Partial<TaskState>,
): TaskState => ({
  id,
  description: `Implement ${id}`,
  dependencies: deps,
  testCommand: opts?.testCommand ?? "npm test",
  status: "pending",
  attempts: 0,
  ...opts,
});

function setupRuntime(
  tasks: TaskState[],
  host: MockRuntimeHost,
): WorkflowContext {
  const runtime = createRuntime();
  const taskMap = new Map(tasks.map((t) => [t.id, t]));
  const ctx = runtime.activate(
    {
      dispatchOpts: { concurrency: 3 },
      state: () => ({ tasks: taskMap, explorations: new Map() }),
    },
    host,
  );
  return ctx;
}

// ── Tests ──────────────────────────────────────────────────────────────

describe("nefor DAG execution pipeline", () => {
  describe("full pipeline: Build -> Review(PASS) -> Test(PASS) -> Done", () => {
    it("runs the complete pipeline for a single task", async () => {
      const effectLog: string[] = [];

      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          await new Promise((r) => setTimeout(r, 10));
          if (config.model === "test-builder")
            return agentResult.pass("Build successful");
          if (config.model === "test-reviewer")
            return agentResult.pass("VERDICT: PASS");
          if (config.model === "test-tester")
            return agentResult.pass("VERDICT: PASS");
          return agentResult.pass();
        },
      });

      const task = makeTask("task-1");
      const ctx = setupRuntime([task], host);

      // Wrap effects to track execution order
      const trackedSchedule: Effect = {
        type: "schedule-ready",
        handle: async (c) => {
          effectLog.push("schedule-ready");
          const result = await ScheduleReady().handle(c);
          return result.map((e) => trackEffect(e));
        },
      };

      function trackEffect(e: Effect): Effect {
        return {
          type: e.type,
          handle: async (c) => {
            effectLog.push(e.type);
            const result = await e.handle(c);
            return result.map((child) => trackEffect(child));
          },
        };
      }

      await dispatch([trackedSchedule], ctx, { concurrency: 3 });

      expect(host.spawnLog).toHaveLength(3);
      expect(host.spawnLog[0].config.model).toBe("test-builder");
      expect(host.spawnLog[1].config.model).toBe("test-reviewer");
      expect(host.spawnLog[2].config.model).toBe("test-tester");

      expect(effectLog).toEqual([
        "schedule-ready",
        "build",
        "review-code",
        "test-code",
        "task-done",
        "schedule-ready",
      ]);

      expect(task.status).toBe("done");
      expect(task.finishedAt).toBeTypeOf("number");
    });
  });

  describe("parallel independent tasks", () => {
    it("runs A and B concurrently, then C after both complete", async () => {
      const spawnTimestamps: Array<{ model: string; time: number }> = [];

      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          spawnTimestamps.push({ model: config.model!, time: Date.now() });
          await new Promise((r) => setTimeout(r, 25));
          if (config.model === "test-builder")
            return agentResult.pass("Build successful");
          if (config.model === "test-reviewer")
            return agentResult.pass("VERDICT: PASS");
          if (config.model === "test-tester")
            return agentResult.pass("VERDICT: PASS");
          return agentResult.pass();
        },
      });

      const taskA = makeTask("A");
      const taskB = makeTask("B");
      const taskC = makeTask("C", ["A", "B"]);
      const ctx = setupRuntime([taskA, taskB, taskC], host);

      await dispatch([ScheduleReady()], ctx, { concurrency: 3 });

      // All 3 tasks should complete
      expect(taskA.status).toBe("done");
      expect(taskB.status).toBe("done");
      expect(taskC.status).toBe("done");

      // 9 total spawns: 3 tasks x 3 stages (build, review, test)
      expect(host.spawnLog).toHaveLength(9);

      // A and B should start concurrently (their first build spawns
      // should happen before either finishes its full pipeline).
      // Find the first builder spawn for each task.
      const builderSpawns = host.spawnLog
        .map((s, i) => ({ ...s, index: i }))
        .filter((s) => s.config.model === "test-builder");

      // A and B builders should be in positions 0 and 1 (started before
      // either's review), while C builder comes after both A and B finish.
      const aFirstBuild = builderSpawns.find((s) =>
        s.prompt.includes("Build A"),
      );
      const bFirstBuild = builderSpawns.find((s) =>
        s.prompt.includes("Build B"),
      );
      const cFirstBuild = builderSpawns.find((s) =>
        s.prompt.includes("Build C"),
      );

      expect(aFirstBuild).toBeDefined();
      expect(bFirstBuild).toBeDefined();
      expect(cFirstBuild).toBeDefined();

      // C's builder must come after both A and B complete their full pipelines
      expect(cFirstBuild!.index).toBeGreaterThan(aFirstBuild!.index);
      expect(cFirstBuild!.index).toBeGreaterThan(bFirstBuild!.index);

      // A and B should start before either finishes completely
      // (indexes 0 and 1 for their builders, or at least close together)
      expect(Math.abs(aFirstBuild!.index - bFirstBuild!.index)).toBeLessThan(3);
    });
  });

  describe("escalation after 3 build failures", () => {
    it("attempts build 3 times then escalates", async () => {
      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          await new Promise((r) => setTimeout(r, 10));
          if (config.model === "test-builder")
            return agentResult.fail("Build error: compilation failed");
          return agentResult.pass();
        },
      });

      const task = makeTask("failing-task");
      const ctx = setupRuntime([task], host);

      await dispatch([ScheduleReady()], ctx, { concurrency: 3 });

      // Builder called 3 times (attempts 1, 2, 3)
      const builderCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-builder",
      );
      expect(builderCalls).toHaveLength(3);

      // No reviewer or tester calls since build never succeeded
      const reviewerCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-reviewer",
      );
      expect(reviewerCalls).toHaveLength(0);

      // Task is failed
      expect(task.status).toBe("failed");

      // sendMessage called with escalation
      expect(host.messages).toHaveLength(1);
      expect(host.messages[0].content).toContain("Escalation");
      expect(host.messages[0].content).toContain("failing-task");
      expect(host.messages[0].opts?.triggerTurn).toBe(true);
    });
  });

  describe("review retry cycle", () => {
    it("retries build when reviewer returns CHANGES_NEEDED, passes on third review", async () => {
      let reviewCount = 0;

      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          await new Promise((r) => setTimeout(r, 10));
          if (config.model === "test-builder")
            return agentResult.pass("Build output");
          if (config.model === "test-reviewer") {
            reviewCount++;
            if (reviewCount < 3) {
              return agentResult.changesNeeded(
                "VERDICT: CHANGES_NEEDED\nFix the formatting",
              );
            }
            return agentResult.pass("VERDICT: PASS");
          }
          if (config.model === "test-tester")
            return agentResult.pass("VERDICT: PASS");
          return agentResult.pass();
        },
      });

      const task = makeTask("retry-task");
      const ctx = setupRuntime([task], host);

      await dispatch([ScheduleReady()], ctx, { concurrency: 3 });

      // Build runs 3 times (initial + 2 retries from CHANGES_NEEDED)
      const builderCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-builder",
      );
      expect(builderCalls).toHaveLength(3);

      // Review runs 3 times
      const reviewerCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-reviewer",
      );
      expect(reviewerCalls).toHaveLength(3);

      // Test runs once (after final PASS review)
      const testerCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-tester",
      );
      expect(testerCalls).toHaveLength(1);

      expect(task.status).toBe("done");
    });
  });

  describe("cascade failure", () => {
    it("cancels dependent tasks when upstream task fails", async () => {
      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          await new Promise((r) => setTimeout(r, 10));
          if (config.model === "test-builder")
            return agentResult.fail("Build failed");
          return agentResult.pass();
        },
      });

      const taskA = makeTask("A");
      const taskB = makeTask("B", ["A"]);
      const taskC = makeTask("C", ["B"]);
      const ctx = setupRuntime([taskA, taskB, taskC], host);

      await dispatch([ScheduleReady()], ctx, { concurrency: 3 });

      // A fails after 3 build attempts
      expect(taskA.status).toBe("failed");

      // B and C are cascade-cancelled without ever running
      expect(taskB.status).toBe("failed");
      expect(taskC.status).toBe("failed");

      // B and C should have never been spawned (no builder calls for them)
      const bBuilds = host.spawnLog.filter((s) =>
        s.prompt.includes("Build B"),
      );
      const cBuilds = host.spawnLog.filter((s) =>
        s.prompt.includes("Build C"),
      );
      expect(bBuilds).toHaveLength(0);
      expect(cBuilds).toHaveLength(0);

      // Only A's builder was called (3 attempts)
      expect(host.spawnLog).toHaveLength(3);

      // Escalation message mentions cascade
      expect(host.messages[0].content).toContain("Cascade-cancelled");
      expect(host.messages[0].content).toContain("B");
      expect(host.messages[0].content).toContain("C");
    });
  });

  describe("async explore alongside DAG", () => {
    it("runs exploration concurrently with DAG task execution", async () => {
      const completionOrder: string[] = [];

      const host = createMockHost({
        onSpawn: async (config, _prompt) => {
          await new Promise((r) => setTimeout(r, 15));
          if (config.model === "test-explorer") {
            completionOrder.push("explorer");
            return agentResult.explorerResult(
              "Found relevant files in src/",
            );
          }
          if (config.model === "test-builder") {
            completionOrder.push("builder");
            return agentResult.pass("Build successful");
          }
          if (config.model === "test-reviewer") {
            completionOrder.push("reviewer");
            return agentResult.pass("VERDICT: PASS");
          }
          if (config.model === "test-tester") {
            completionOrder.push("tester");
            return agentResult.pass("VERDICT: PASS");
          }
          return agentResult.pass();
        },
      });

      const task = makeTask("dag-task");
      const ctx = setupRuntime([task], host);

      // Fire-and-forget exploration effect
      const AsyncExplore = (): Effect => ({
        type: "async-explore",
        handle: async (c) => {
          const result = await c.spawn(
            explorerAgent,
            "Find all utility files",
          );
          // Store result in state for assertion
          c.state.exploreResult = result.output;
          return [];
        },
      });

      // Run both the explore and the DAG schedule concurrently
      await dispatch([AsyncExplore(), ScheduleReady()], ctx, {
        concurrency: 3,
      });

      // DAG task completed
      expect(task.status).toBe("done");

      // Exploration result was captured
      expect(ctx.state.exploreResult).toContain("Found relevant files");

      // Both explorer and DAG agents were spawned
      const explorerCalls = host.spawnLog.filter(
        (s) => s.config.model === "test-explorer",
      );
      expect(explorerCalls).toHaveLength(1);

      // DAG pipeline ran fully (builder + reviewer + tester)
      const dagCalls = host.spawnLog.filter(
        (s) => s.config.model !== "test-explorer",
      );
      expect(dagCalls).toHaveLength(3);
    });
  });
});
