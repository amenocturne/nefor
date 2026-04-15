/**
 * Loop Guard — Pi Extension
 *
 * Detects duplicate tool calls using a hash-based sliding window and
 * compresses identical results to save tokens.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { createHash } from "node:crypto";
import { readFileSync } from "fs";
import { dirname, join } from "path";
import { fileURLToPath } from "url";
import { parse } from "yaml";

const __dirname = dirname(fileURLToPath(import.meta.url));

// ── Types ───────────────────────────────────────────────────────────────

type CallVerdict = "new" | "same_args_new_result" | "duplicate";

interface Config {
  window_size: number;
  messages: {
    duplicate: string;
  };
}

// ── ToolCallWindow ─────────────────────────────────────────────────────

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
      // Re-insert to refresh LRU position
      this.entries.delete(argsHash);
      this.entries.set(argsHash, resultHash);
      return "same_args_new_result";
    }

    // Refresh LRU position for duplicates too
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

// ── Helpers ─────────────────────────────────────────────────────────────

function sha256(input: string): string {
  return createHash("sha256").update(input).digest("hex");
}

function summarizeArgs(input: unknown): string {
  const str = JSON.stringify(input);
  return str.length > 100 ? str.slice(0, 100) + "..." : str;
}

function loadConfig(): Config {
  try {
    const raw = readFileSync(join(__dirname, "config.yaml"), "utf-8");
    const parsed = parse(raw);
    return {
      window_size: parsed?.window_size ?? 20,
      messages: {
        duplicate: parsed?.messages?.duplicate ?? "Result identical to your previous {tool}({args_summary}) call. The data hasn't changed. Consider a different approach.",
      },
    };
  } catch {
    return {
      window_size: 20,
      messages: {
        duplicate: "Result identical to your previous {tool}({args_summary}) call. The data hasn't changed. Consider a different approach.",
      },
    };
  }
}

function formatMessage(template: string, toolName: string, argsSummary: string): string {
  return template.replace("{tool}", toolName).replace("{args_summary}", argsSummary);
}

// ── Extension Entry ─────────────────────────────────────────────────────

export default function (pi: ExtensionAPI) {
  const config = loadConfig();
  const window = new ToolCallWindow(config.window_size);

  pi.on("input", async () => {
    window.reset();
    return { action: "continue" as const };
  });

  pi.on("tool_result", async (event) => {
    const verdict = window.record(event.toolName, event.input, event.content);

    switch (verdict) {
      case "new":
      case "same_args_new_result":
        return;

      case "duplicate": {
        const argsSummary = summarizeArgs(event.input);
        const message = formatMessage(config.messages.duplicate.trim(), event.toolName, argsSummary);
        return {
          content: [{
            type: "text" as const,
            text: message,
          }],
          isError: event.isError,
        };
      }
    }
  });
}
