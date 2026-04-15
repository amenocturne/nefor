import { describe, it, expect } from "vitest";
import { createHash } from "node:crypto";

// Inline the class for testing since it's not exported separately
type CallVerdict = "new" | "same_args_new_result" | "duplicate";

class ToolCallWindow {
  private readonly windowSize: number;
  private entries = new Map<string, string>();

  constructor(windowSize = 20) {
    this.windowSize = windowSize;
  }

  record(toolName: string, args: unknown, result: unknown): CallVerdict {
    const argsHash = sha256(toolName + ":" + JSON.stringify(args));
    const resultHash = sha256(JSON.stringify(result));

    const existingResultHash = this.entries.get(argsHash);

    if (existingResultHash === undefined) {
      this.evictIfNeeded();
      this.entries.set(argsHash, resultHash);
      return "new";
    }

    if (existingResultHash !== resultHash) {
      this.entries.delete(argsHash);
      this.entries.set(argsHash, resultHash);
      return "same_args_new_result";
    }

    this.entries.delete(argsHash);
    this.entries.set(argsHash, resultHash);
    return "duplicate";
  }

  reset(): void {
    this.entries.clear();
  }

  private evictIfNeeded(): void {
    if (this.entries.size >= this.windowSize) {
      const oldest = this.entries.keys().next().value!;
      this.entries.delete(oldest);
    }
  }
}

function sha256(input: string): string {
  return createHash("sha256").update(input).digest("hex");
}

describe("ToolCallWindow", () => {
  it("returns 'new' for first call", () => {
    const w = new ToolCallWindow();
    expect(w.record("read", { path: "/foo.ts" }, "file contents")).toBe("new");
  });

  it("returns 'duplicate' for same args and same result", () => {
    const w = new ToolCallWindow();
    w.record("read", { path: "/foo.ts" }, "file contents");
    expect(w.record("read", { path: "/foo.ts" }, "file contents")).toBe("duplicate");
  });

  it("returns 'same_args_new_result' when result changes", () => {
    const w = new ToolCallWindow();
    w.record("read", { path: "/foo.ts" }, "version 1");
    expect(w.record("read", { path: "/foo.ts" }, "version 2")).toBe("same_args_new_result");
  });

  it("returns 'new' after reset", () => {
    const w = new ToolCallWindow();
    w.record("read", { path: "/foo.ts" }, "contents");
    w.reset();
    expect(w.record("read", { path: "/foo.ts" }, "contents")).toBe("new");
  });

  it("evicts oldest entry when window is full", () => {
    const w = new ToolCallWindow(3);
    w.record("read", { path: "/a" }, "a");
    w.record("read", { path: "/b" }, "b");
    w.record("read", { path: "/c" }, "c");
    // Window full, next insert evicts /a
    w.record("read", { path: "/d" }, "d");
    // /a was evicted, so it's "new" again
    expect(w.record("read", { path: "/a" }, "a")).toBe("new");
    // /b was evicted when /a was re-inserted (window: /c, /d, /a)
    expect(w.record("read", { path: "/b" }, "b")).toBe("new");
    // /d should still be there (window: /d, /a, /b — /c evicted)
    expect(w.record("read", { path: "/d" }, "d")).toBe("duplicate");
  });

  it("treats different arg order as different calls", () => {
    const w = new ToolCallWindow();
    w.record("bash", { a: 1, b: 2 }, "output");
    // Different key order = different JSON.stringify = different hash
    expect(w.record("bash", { b: 2, a: 1 }, "output")).toBe("new");
  });

  it("treats different tools with same args as different calls", () => {
    const w = new ToolCallWindow();
    w.record("read", { path: "/foo" }, "output");
    expect(w.record("grep", { path: "/foo" }, "output")).toBe("new");
  });

  it("tracks duplicate correctly after a same_args_new_result", () => {
    const w = new ToolCallWindow();
    w.record("read", { path: "/foo" }, "v1");
    w.record("read", { path: "/foo" }, "v2"); // same_args_new_result
    // Now the stored result hash is for "v2"
    expect(w.record("read", { path: "/foo" }, "v2")).toBe("duplicate");
    expect(w.record("read", { path: "/foo" }, "v1")).toBe("same_args_new_result");
  });
});
