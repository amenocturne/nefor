/**
 * E2E tests: spawn a real Pi process against a mock LLM server.
 * Validates Pi startup, provider routing, tool use, and extension loading.
 */
import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { spawn } from "node:child_process";
import { writeFileSync, readFileSync, mkdirSync, unlinkSync, existsSync, rmSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { createMockLLMServer, type ResponseHandler } from "./mock-llm-server.ts";

// ── Constants ───────────────────────────────────────────────────────────

const PI_BIN = "/opt/homebrew/bin/pi";
const MODELS_JSON_PATH = join(homedir(), ".pi", "agent", "models.json");
const EXTENSION_PATH = join(import.meta.dirname, "../../extensions/disguise");
const E2E_TEST_FILE = "/tmp/test-e2e-file.txt";
const E2E_TEST_CONTENT = "Hello from e2e test — mock LLM should read this file.";

// ── Helpers ─────────────────────────────────────────────────────────────

function runPi(args: string[], env?: Record<string, string>): Promise<{
  stdout: string;
  stderr: string;
  exitCode: number;
  events: any[];
}> {
  return new Promise((resolve) => {
    const proc = spawn(PI_BIN, args, {
      env: { ...process.env, ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";

    proc.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString(); });
    proc.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });

    proc.on("close", (code) => {
      const events = stdout.split("\n")
        .filter((line) => line.trim())
        .map((line) => { try { return JSON.parse(line); } catch { return null; } })
        .filter(Boolean);
      resolve({ stdout, stderr, exitCode: code ?? 1, events });
    });
  });
}

function writeModelsJson(port: number) {
  const config = {
    providers: {
      "mock-llm": {
        baseUrl: `http://127.0.0.1:${port}/v1`,
        apiKey: "test-key",
        api: "openai-completions",
        models: [
          {
            id: "mock-model",
            name: "Mock Model",
            contextWindow: 128000,
            maxTokens: 4096,
            reasoning: false,
            input: ["text"],
          },
        ],
        compat: {
          maxTokensField: "max_tokens",
        },
      },
    },
  };

  mkdirSync(join(homedir(), ".pi", "agent"), { recursive: true });
  writeFileSync(MODELS_JSON_PATH, JSON.stringify(config, null, 2));
}

// ── Setup / Teardown ────────────────────────────────────────────────────

let modelsJsonBackup: string | null = null;

beforeAll(() => {
  // Back up existing models.json
  if (existsSync(MODELS_JSON_PATH)) {
    modelsJsonBackup = readFileSync(MODELS_JSON_PATH, "utf-8");
  }

  // Create test file for tool-use test
  writeFileSync(E2E_TEST_FILE, E2E_TEST_CONTENT);
});

afterAll(() => {
  // Restore models.json
  if (modelsJsonBackup !== null) {
    writeFileSync(MODELS_JSON_PATH, modelsJsonBackup);
  } else if (existsSync(MODELS_JSON_PATH)) {
    unlinkSync(MODELS_JSON_PATH);
  }

  // Clean up test file
  if (existsSync(E2E_TEST_FILE)) {
    unlinkSync(E2E_TEST_FILE);
  }
});

// ── Tests ───────────────────────────────────────────────────────────────

describe("Pi E2E with mock LLM", { timeout: 60_000 }, () => {

  it("basic text response: Pi communicates with mock server", async () => {
    const handler: ResponseHandler = () => ({
      type: "text",
      content: "Hello from mock LLM!",
    });

    const server = createMockLLMServer(handler);
    const { port } = await server.start();

    try {
      writeModelsJson(port);

      const result = await runPi([
        "--provider", "mock-llm",
        "--model", "mock-model",
        "--mode", "json",
        "-p",
        "--no-session",
        "--no-extensions",
        "--tools", "read",
        "Say hello",
      ]);

      // Debug output on failure
      if (result.exitCode !== 0) {
        console.error("Pi stderr:", result.stderr);
        console.error("Pi stdout:", result.stdout);
        console.error("Request log:", JSON.stringify(server.requestLog, null, 2));
      }

      expect(result.exitCode).toBe(0);
      expect(server.requestLog.length).toBe(1);

      // Look for assistant text in JSONL output
      const messageEvents = result.events.filter(
        (e) => e.type === "message_update" || e.type === "assistant" || e.type === "content",
      );
      // At minimum, Pi should have produced some output events
      expect(result.events.length).toBeGreaterThan(0);

      // The mock response text should appear somewhere in stdout
      expect(result.stdout).toContain("Hello from mock LLM!");
    } finally {
      await server.stop();
    }
  });

  it("tool use: Pi executes read tool when mock requests it", async () => {
    let callCount = 0;

    const handler: ResponseHandler = (messages) => {
      callCount++;
      // First call: request a file read
      if (callCount === 1) {
        return {
          type: "tool_call",
          name: "read",
          arguments: { path: E2E_TEST_FILE },
        };
      }
      // Second call (after tool result): return final text
      return {
        type: "text",
        content: "I read the file successfully",
      };
    };

    const server = createMockLLMServer(handler);
    const { port } = await server.start();

    try {
      writeModelsJson(port);

      const result = await runPi([
        "--provider", "mock-llm",
        "--model", "mock-model",
        "--mode", "json",
        "-p",
        "--no-session",
        "--no-extensions",
        "--tools", "read",
        "Read the test file",
      ]);

      if (result.exitCode !== 0) {
        console.error("Pi stderr:", result.stderr);
        console.error("Pi stdout:", result.stdout);
        console.error("Request log:", JSON.stringify(server.requestLog, null, 2));
      }

      expect(result.exitCode).toBe(0);

      // Mock server should have received 2 requests: initial + after tool result
      expect(server.requestLog.length).toBe(2);

      // Second request should contain tool result in messages
      const secondRequest = server.requestLog[1];
      const toolMessages = secondRequest.messages.filter(
        (m: any) => m.role === "tool",
      );
      expect(toolMessages.length).toBeGreaterThan(0);

      // Final output should contain the mock's text response
      expect(result.stdout).toContain("I read the file successfully");
    } finally {
      await server.stop();
    }
  });

  it("extension loading: Pi starts with disguise extension without crashing", async () => {
    const handler: ResponseHandler = () => ({
      type: "text",
      content: "Extension loaded OK",
    });

    const server = createMockLLMServer(handler);
    const { port } = await server.start();

    try {
      writeModelsJson(port);

      const result = await runPi([
        "--provider", "mock-llm",
        "--model", "mock-model",
        "--mode", "json",
        "-p",
        "--no-session",
        "--no-extensions",
        "-e", EXTENSION_PATH,
        "--tools", "read",
        "Hello",
      ]);

      if (result.exitCode !== 0) {
        console.error("Pi stderr:", result.stderr);
        console.error("Pi stdout:", result.stdout);
      }

      expect(result.exitCode).toBe(0);
      expect(result.events.length).toBeGreaterThan(0);
    } finally {
      await server.stop();
    }
  });
});
