import { EventEmitter } from "events";
import { beforeEach, describe, expect, it, vi } from "vitest";

const spawnMock = vi.fn();

vi.mock("child_process", () => ({
  spawn: spawnMock,
}));

type MockProc = EventEmitter & {
  stdout: EventEmitter & { setEncoding: (encoding: string) => void };
  stderr: EventEmitter & { setEncoding: (encoding: string) => void };
  pid: number;
};

const makeProc = (): MockProc => {
  const proc = new EventEmitter() as MockProc;
  proc.pid = 123;
  proc.stdout = Object.assign(new EventEmitter(), { setEncoding: vi.fn() });
  proc.stderr = Object.assign(new EventEmitter(), { setEncoding: vi.fn() });
  return proc;
};

describe("spawnAgent provider routing", () => {
  beforeEach(() => {
    spawnMock.mockReset();
    spawnMock.mockReturnValue(makeProc());
  });

  it("passes canonical Codex provider for prefixed model ids", async () => {
    const { spawnAgent } = await import("../../lib/task-manager.ts");

    await spawnAgent(
      "t1",
      "Do work",
      "codex/gpt-5.4",
      undefined,
      process.cwd(),
      ["/tmp/ext-a"],
    );

    expect(spawnMock).toHaveBeenCalledWith(
      "pi",
      expect.arrayContaining([
        "--provider",
        "openai-codex",
        "--model",
        "gpt-5.4",
      ]),
      expect.any(Object),
    );
  });

  it("strips openrouter prefix before spawning", async () => {
    const { spawnAgent } = await import("../../lib/task-manager.ts");

    await spawnAgent(
      "t2",
      "Explore",
      "openrouter/anthropic/claude-sonnet-4.6",
      undefined,
      process.cwd(),
      [],
    );

    expect(spawnMock).toHaveBeenCalledWith(
      "pi",
      expect.arrayContaining([
        "--provider",
        "openrouter",
        "--model",
        "anthropic/claude-sonnet-4.6",
      ]),
      expect.any(Object),
    );
  });

  it("passes explicit provider for bare model ids", async () => {
    const { spawnAgent } = await import("../../lib/task-manager.ts");

    await spawnAgent(
      "t3",
      "Build",
      "gpt-5.4",
      "openai-codex",
      process.cwd(),
      [],
    );

    expect(spawnMock).toHaveBeenCalledWith(
      "pi",
      expect.arrayContaining([
        "--provider",
        "openai-codex",
        "--model",
        "gpt-5.4",
      ]),
      expect.any(Object),
    );
  });

  it("leaves bare model ids unchanged", async () => {
    const { spawnAgent } = await import("../../lib/task-manager.ts");

    await spawnAgent("t4", "Build", "gpt-5.4", undefined, process.cwd(), []);

    const [, args] = spawnMock.mock.calls[0];
    expect(args).toContain("--model");
    expect(args).toContain("gpt-5.4");
    expect(args).not.toContain("--provider");
  });
});
