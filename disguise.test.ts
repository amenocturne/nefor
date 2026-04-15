import { vi, describe, it, expect, beforeEach } from "vitest";

vi.mock("./lib/workflow/index.ts", () => ({
  defineDisguise: (config: any) => config,
}));

vi.mock("./lib/task-manager.ts", () => ({
  killTask: vi.fn(),
  killAllTasks: vi.fn(),
  getTask: vi.fn(),
}));

vi.mock("./lib/hooks.ts", () => ({
  loadHooksConfig: vi.fn(() => ({ hooks: {} })),
  runPlanHooks: vi.fn().mockResolvedValue(null),
}));

vi.mock("./config/index.ts", () => ({
  default: {
    models: {
      orchestrator: "mock-orchestrator",
      explorer: "mock-explorer",
      worker: "mock-worker",
      reviewer: "mock-reviewer",
      tester: "mock-tester",
      promptEngineer: "mock-prompt-engineer",
    },
  },
}));

import disguise from "./disguise.ts";
import {
  createMockContext,
  agentResult,
  type MockWorkflowContext,
} from "./tests/utils/mock-context.ts";

const bgPlan = disguise.lead.tools["bg-plan"];
const terminate = disguise.lead.tools.terminate;

describe("bg-plan tool", () => {
  let ctx: MockWorkflowContext;

  beforeEach(() => {
    vi.clearAllMocks();
    ctx = createMockContext({
      state: {
        nodes: new Map(),
        planApproved: true,
      },
    });
  });

  it("rejects write agents when plan is not approved", async () => {
    ctx.state.planApproved = false;
    const result = await bgPlan.execute({
      nodes: [{ id: "test", description: "do stuff", agentType: "builder", workDir: "." }],
    }, ctx);
    expect(result).toContain("not yet approved");
  });

  it("allows read-only agents without plan approval", async () => {
    ctx.state.planApproved = false;
    const result = await bgPlan.execute({
      nodes: [
        { id: "exp", description: "explore", agentType: "explorer", workDir: "." },
        { id: "crit", description: "critique", agentType: "critic", workDir: "." },
      ],
    }, ctx);
    expect(result).toContain("2 nodes submitted");
  });

  it("creates nodes in state from submitted plan", async () => {
    await bgPlan.execute({
      nodes: [
        { id: "build-auth", description: "Add auth", agentType: "builder", workDir: "src" },
        { id: "review-auth", description: "Review auth", agentType: "reviewer", dependencies: ["build-auth"], workDir: "src" },
      ],
    }, ctx);

    const nodes = ctx.state.nodes as Map<string, any>;
    expect(nodes.size).toBe(2);

    const build = nodes.get("build-auth");
    expect(build.agentType).toBe("builder");
    expect(build.status).toBe("pending");
    expect(build.dependencies).toEqual([]);
    expect(build.maxAttempts).toBe(3);

    const review = nodes.get("review-auth");
    expect(review.agentType).toBe("reviewer");
    expect(review.dependencies).toEqual(["build-auth"]);
  });

  it("dispatches ScheduleReady after submitting", async () => {
    const result = await bgPlan.execute({
      nodes: [{ id: "test", description: "do stuff", agentType: "builder", workDir: "." }],
    }, ctx);

    expect(ctx.dispatch).toHaveBeenCalledTimes(1);
    expect(result).toContain("1 nodes submitted");
  });

  it("defaults agentType to builder and maxAttempts to 3", async () => {
    await bgPlan.execute({
      nodes: [{ id: "test", description: "do stuff", workDir: "." }],
    }, ctx);

    const nodes = ctx.state.nodes as Map<string, any>;
    const node = nodes.get("test");
    expect(node.agentType).toBe("builder");
    expect(node.maxAttempts).toBe(3);
  });

  it("respects custom maxAttempts", async () => {
    await bgPlan.execute({
      nodes: [{ id: "test", description: "do stuff", agentType: "builder", workDir: ".", maxAttempts: 1 }],
    }, ctx);

    const nodes = ctx.state.nodes as Map<string, any>;
    expect(nodes.get("test").maxAttempts).toBe(1);
  });
});

describe("terminate tool", () => {
  let ctx: MockWorkflowContext;

  beforeEach(() => {
    vi.clearAllMocks();
    ctx = createMockContext({
      state: {
        nodes: new Map([
          ["running-node", {
            id: "running-node", description: "test", agentType: "builder",
            dependencies: [], workDir: ".", status: "running", attempts: 1,
            maxAttempts: 3, processId: "t1", startedAt: Date.now(),
          }],
          ["pending-node", {
            id: "pending-node", description: "test2", agentType: "builder",
            dependencies: ["running-node"], workDir: ".", status: "pending",
            attempts: 0, maxAttempts: 3,
          }],
        ]),
        planApproved: true,
      },
    });
  });

  it("terminates a specific node by ID", async () => {
    const result = await terminate.execute({ id: "running-node", reason: "testing" }, ctx);
    const nodes = ctx.state.nodes as Map<string, any>;
    expect(nodes.get("running-node").status).toBe("failed");
    expect(result).toContain("running-node");
  });

  it("terminates all nodes", async () => {
    const result = await terminate.execute({ id: "all", reason: "abort" }, ctx);
    const nodes = ctx.state.nodes as Map<string, any>;
    expect(nodes.get("running-node").status).toBe("failed");
    expect(nodes.get("pending-node").status).toBe("failed");
    expect(result).toContain("2 node(s)");
  });

  it("returns error for non-existent node", async () => {
    const result = await terminate.execute({ id: "nope", reason: "test" }, ctx);
    expect(result).toContain("not found");
  });
});
