import { describe, it, expect, vi } from "vitest";
import { dispatch } from "./runtime.ts";
import type { Effect, WorkflowContext } from "./types.ts";
import { createMockContext } from "../../tests/utils/mock-context.ts";

// Helper: create an effect that records execution order and optionally returns children
function makeEffect(
  name: string,
  log: string[],
  opts?: { delayMs?: number; children?: Effect[]; throws?: Error; priority?: number },
): Effect {
  return {
    type: name,
    priority: opts?.priority,
    async handle(_ctx: WorkflowContext): Promise<Effect[]> {
      log.push(`${name}:start`);
      if (opts?.delayMs) {
        await new Promise((r) => setTimeout(r, opts.delayMs));
      }
      if (opts?.throws) {
        log.push(`${name}:throw`);
        throw opts.throws;
      }
      log.push(`${name}:end`);
      return opts?.children ?? [];
    },
  };
}

describe("dispatch()", () => {
  it("runs effects sequentially at concurrency=1", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    const e1 = makeEffect("e1", log, { delayMs: 30 });
    const e2 = makeEffect("e2", log, { delayMs: 20 });

    await dispatch([e1, e2], ctx, { concurrency: 1 });

    expect(log).toEqual(["e1:start", "e1:end", "e2:start", "e2:end"]);
  });

  it("runs effects in parallel at concurrency>1", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    const e1 = makeEffect("e1", log, { delayMs: 40 });
    const e2 = makeEffect("e2", log, { delayMs: 40 });

    await dispatch([e1, e2], ctx, { concurrency: 2 });

    // Both should start before either ends
    const e1Start = log.indexOf("e1:start");
    const e2Start = log.indexOf("e2:start");
    const e1End = log.indexOf("e1:end");
    const e2End = log.indexOf("e2:end");

    expect(e1Start).toBeLessThan(e1End);
    expect(e2Start).toBeLessThan(e2End);
    // Both start before either finishes
    expect(e1Start).toBeLessThan(e2End);
    expect(e2Start).toBeLessThan(e1End);
  });

  it("chains child effects from parent", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    const child1 = makeEffect("child1", log);
    const child2 = makeEffect("child2", log);
    const parent = makeEffect("parent", log, { children: [child1, child2] });

    await dispatch([parent], ctx, { concurrency: 1 });

    expect(log).toEqual([
      "parent:start",
      "parent:end",
      "child1:start",
      "child1:end",
      "child2:start",
      "child2:end",
    ]);
  });

  it("continues processing after an effect throws", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    const bad = makeEffect("bad", log, { throws: new Error("boom") });
    const good = makeEffect("good", log);

    await dispatch([bad, good], ctx, { concurrency: 1 });

    // Bad effect started and threw, good effect still ran
    expect(log).toContain("bad:start");
    expect(log).toContain("bad:throw");
    expect(log).toContain("good:start");
    expect(log).toContain("good:end");

    // ctx.log was called with the error
    expect(ctx.log).toHaveBeenCalledWith(
      expect.stringContaining("effect bad threw"),
    );
  });

  it("breaks on queue overflow when maxQueueDepth is exceeded", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    // Create an effect that spawns more children, causing the queue to grow
    function spawner(depth: number): Effect {
      return {
        type: `spawner-${depth}`,
        async handle(): Promise<Effect[]> {
          log.push(`spawner-${depth}`);
          // Each spawner adds 3 more effects
          return [
            makeEffect(`child-${depth}-a`, log),
            makeEffect(`child-${depth}-b`, log),
            makeEffect(`child-${depth}-c`, log),
          ];
        },
      };
    }

    // Start with 3 spawners, maxQueueDepth=2
    // After the first spawner runs it pushes 3 children, queue becomes [spawner1, spawner2, childA, childB, childC] = 5 items
    // That exceeds maxQueueDepth=2, so dispatch should break
    await dispatch(
      [spawner(0), spawner(1), spawner(2)],
      ctx,
      { concurrency: 1, maxQueueDepth: 2 },
    );

    // ctx.log was called with overflow message
    expect(ctx.log).toHaveBeenCalledWith(
      expect.stringContaining("queue overflow"),
    );

    // Not all effects ran — the queue was cut short
    const totalRuns = log.length;
    // Without the limit, we'd have 3 spawners + 9 children = 12 entries
    expect(totalRuns).toBeLessThan(12);
  });

  it("picks higher-priority effects before lower-priority ones", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    const low = makeEffect("low", log, { priority: 10 });
    const high = makeEffect("high", log, { priority: 80 });
    const mid = makeEffect("mid", log, { priority: 40 });

    // Queue order: low, high, mid — but high should run first
    await dispatch([low, high, mid], ctx, { concurrency: 1 });

    expect(log).toEqual([
      "high:start", "high:end",
      "mid:start", "mid:end",
      "low:start", "low:end",
    ]);
  });

  it("prioritizes child effects from high-priority parents (depth-first)", async () => {
    const ctx = createMockContext();
    const log: string[] = [];

    // Simulates: build spawns review (high priority), but another build is queued
    const review = makeEffect("review", log, { priority: 60 });
    const build1 = makeEffect("build1", log, { priority: 20, children: [review] });
    const build2 = makeEffect("build2", log, { priority: 20 });

    await dispatch([build1, build2], ctx, { concurrency: 1 });

    // build1 runs first (same priority, first in queue), spawns review
    // review (priority 60) should run before build2 (priority 20)
    expect(log).toEqual([
      "build1:start", "build1:end",
      "review:start", "review:end",
      "build2:start", "build2:end",
    ]);
  });

  it("respects minDelayMs between effects", async () => {
    const ctx = createMockContext();
    const timestamps: number[] = [];
    const delayMs = 50;

    const timedEffect = (name: string): Effect => ({
      type: name,
      async handle(): Promise<Effect[]> {
        timestamps.push(Date.now());
        return [];
      },
    });

    const e1 = timedEffect("t1");
    const e2 = timedEffect("t2");
    const e3 = timedEffect("t3");

    await dispatch([e1, e2, e3], ctx, { concurrency: 1, minDelayMs: delayMs });

    expect(timestamps).toHaveLength(3);
    // Each effect should be separated by at least ~delayMs
    // Allow 10ms tolerance for timer imprecision
    const gap1 = timestamps[1] - timestamps[0];
    const gap2 = timestamps[2] - timestamps[1];
    expect(gap1).toBeGreaterThanOrEqual(delayMs - 10);
    expect(gap2).toBeGreaterThanOrEqual(delayMs - 10);
  });
});
