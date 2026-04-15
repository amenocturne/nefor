/**
 * Shared mock factories for WorkflowContext and RuntimeHost.
 * Used across unit and integration tests.
 */
import { vi } from "vitest";
import type {
  WorkflowContext,
  RuntimeHost,
  AgentConfig,
  AgentResult,
  SpawnOpts,
  DisguiseConfig,
  MessageOpts,
} from "../../lib/workflow/types.ts";

// ── WorkflowContext mock (for unit tests) ──────────────────────────────

export type MockWorkflowContext = WorkflowContext & {
  /** All sendMessage calls captured for assertions */
  messages: Array<{ content: string; opts?: MessageOpts }>;
  /** Access the underlying vi.fn() for spawn */
  spawn: ReturnType<typeof vi.fn>;
  sendMessage: ReturnType<typeof vi.fn>;
  log: ReturnType<typeof vi.fn>;
  dispatch: ReturnType<typeof vi.fn>;
  setWidget: ReturnType<typeof vi.fn>;
  select: ReturnType<typeof vi.fn>;
};

export function createMockContext(
  overrides: Partial<WorkflowContext> & { config?: Partial<DisguiseConfig> } = {},
): MockWorkflowContext {
  const messages: Array<{ content: string; opts?: MessageOpts }> = [];

  const sendMessage = vi.fn((content: string, opts?: MessageOpts) => {
    messages.push({ content, opts });
  });

  const ctx: MockWorkflowContext = {
    config: { dispatchOpts: { concurrency: 3 }, ...overrides.config } as DisguiseConfig,
    workDir: overrides.workDir ?? "/test/workspace",
    piDir: overrides.piDir ?? "/test/.pi",
    state: overrides.state ?? {},
    messages,
    sendMessage,
    log: vi.fn(),
    spawn: vi.fn(),
    skill: vi.fn().mockResolvedValue({ stdout: "", exitCode: 0 }),
    backgroundSkill: vi.fn(),
    openUrl: vi.fn(),
    registerTool: vi.fn(),
    removeTool: vi.fn(),
    setActiveTools: vi.fn(),
    input: vi.fn().mockResolvedValue(""),
    dispatch: vi.fn(),
    setWidget: vi.fn(),
    select: vi.fn(),
  };

  return ctx;
}

// ── RuntimeHost mock (for integration tests) ────────────────────────────

export type SpawnHandler = (
  config: AgentConfig,
  prompt: string,
  opts?: SpawnOpts,
) => Promise<AgentResult>;

export type MockRuntimeHost = RuntimeHost & {
  messages: Array<{ content: string; opts?: MessageOpts }>;
  widgets: Map<string, string[] | undefined>;
  spawnLog: Array<{ config: AgentConfig; prompt: string; opts?: SpawnOpts }>;
};

const defaultSpawnHandler: SpawnHandler = async (config) => ({
  ok: true,
  output: `mock output for ${config.model ?? "unknown"}`,
  errors: "",
  filesChanged: [],
  exitCode: 0,
});

export function createMockHost(opts: {
  cwd?: string;
  piDir?: string;
  onSpawn?: SpawnHandler;
} = {}): MockRuntimeHost {
  const messages: Array<{ content: string; opts?: MessageOpts }> = [];
  const widgets = new Map<string, string[] | undefined>();
  const spawnLog: Array<{ config: AgentConfig; prompt: string; opts?: SpawnOpts }> = [];
  const spawnHandler = opts.onSpawn ?? defaultSpawnHandler;

  return {
    cwd: opts.cwd ?? "/test/workspace",
    piDir: opts.piDir ?? "/test/.pi",
    messages,
    widgets,
    spawnLog,

    registerTool: vi.fn(),
    removeTool: vi.fn(),
    setActiveTools: vi.fn(),
    getActiveTools: () => [],
    sendMessage: vi.fn((content: string, msgOpts?: MessageOpts) => {
      messages.push({ content, opts: msgOpts });
    }),
    appendSystemPrompt: vi.fn(),

    async spawnAgent(config, prompt, spawnOpts) {
      spawnLog.push({ config, prompt, opts: spawnOpts });
      return spawnHandler(config, prompt, spawnOpts);
    },

    discoverSkills: () => new Map(),
    runSkill: vi.fn().mockResolvedValue({ stdout: "", exitCode: 0 }),
    runSkillBackground: () => null,
    openUrl: vi.fn(),
    onToolCall: vi.fn(),
    input: vi.fn().mockResolvedValue(""),
    select: vi.fn().mockResolvedValue(undefined),
    setWidget: vi.fn((key: string, lines: string[] | undefined) => {
      widgets.set(key, lines);
    }),
  };
}

// ── Agent result helpers ────────────────────────────────────────────────

export const agentResult = {
  pass(output = "VERDICT: PASS"): AgentResult {
    return { ok: true, output, errors: "", filesChanged: [], exitCode: 0 };
  },
  fail(output = "VERDICT: FAIL"): AgentResult {
    return { ok: false, output, errors: "", filesChanged: [], exitCode: 1 };
  },
  changesNeeded(feedback = "VERDICT: CHANGES_NEEDED\nFix line 42"): AgentResult {
    return { ok: true, output: feedback, errors: "", filesChanged: [], exitCode: 0 };
  },
  explorerResult(summary = "Found 3 files matching query"): AgentResult {
    return { ok: true, output: summary, errors: "", filesChanged: [], exitCode: 0 };
  },
};

// ── Timing helpers ──────────────────────────────────────────────────────

/** Flush all pending microtasks (resolved promise callbacks) */
export const flushMicrotasks = () => new Promise<void>((r) => setTimeout(r, 0));

/** Create a deferred promise for controlling spawn timing in tests */
export function deferred<T>(): {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
} {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}
