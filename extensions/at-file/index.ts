/**
 * At-File — interactive @file inclusion for Pi
 *
 * When user types @path/to/file in their message, the extension reads
 * the file and appends its content as a separate non-displayed message
 * after the user's input. The user's original message stays clean —
 * @references remain as-is for readability.
 *
 * Supports:
 *   @relative/path.ts     — relative to cwd
 *   @/absolute/path.ts    — absolute paths
 *   @~/home/path.ts       — home directory expansion
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { setSegment } from "../statusline/index.ts";
import { existsSync, readFileSync, statSync } from "fs";
import { resolve } from "path";
import { homedir } from "os";

// Match @path references — path must contain a slash or dot to avoid
// matching @mentions. Stops at whitespace, comma, or end of string.
const AT_FILE_RE = /(?:^|(?<=\s))@((?:~\/|\.\/|\.\.\/|\/)[^\s,]+|[^\s,]+\.[^\s,]+)/g;

const MAX_CONTENT_LINES = 500;
const MAX_CONTENT_BYTES = 50_000; // ~50KB — roughly 12k tokens

function resolvePath(ref: string, cwd: string): string {
  if (ref.startsWith("~/")) return resolve(homedir(), ref.slice(2));
  return resolve(cwd, ref);
}

function truncateContent(content: string): { text: string; truncated: boolean } {
  const totalLines = content.split("\n").length;

  // Truncate by size first
  if (content.length > MAX_CONTENT_BYTES) {
    const clean = content.slice(0, MAX_CONTENT_BYTES);
    const keptLines = clean.split("\n").length;
    return {
      text: clean + `\n\n... (truncated at ~${Math.round(MAX_CONTENT_BYTES / 1024)}KB — ${totalLines - keptLines} more lines, use read tool with offset to see the rest)`,
      truncated: true,
    };
  }

  // Then by line count
  const lines = content.split("\n");
  if (lines.length <= MAX_CONTENT_LINES) return { text: content, truncated: false };
  return {
    text: lines.slice(0, MAX_CONTENT_LINES).join("\n") + `\n\n... (${totalLines - MAX_CONTENT_LINES} more lines truncated)`,
    truncated: true,
  };
}

export default function (pi: ExtensionAPI) {
  let cwd = ".";

  pi.on("session_start", async (_event, ctx) => {
    cwd = ctx.cwd;
  });

  pi.on("input", async (event, ctx) => {
    const text = event.text;
    if (!text) return { action: "continue" as const };

    const matches = [...text.matchAll(AT_FILE_RE)];
    if (matches.length === 0) return { action: "continue" as const };

    // Collect file contents
    const files: Array<{ ref: string; path: string; content: string; truncated: boolean }> = [];
    const errors: string[] = [];

    for (const match of matches) {
      const ref = match[1];
      const absPath = resolvePath(ref, cwd);

      if (!existsSync(absPath)) {
        errors.push(`@${ref}: file not found`);
        continue;
      }

      try {
        const stat = statSync(absPath);
        if (!stat.isFile()) {
          errors.push(`@${ref}: not a file`);
          continue;
        }

        const raw = readFileSync(absPath, "utf-8");
        const { text: content, truncated } = truncateContent(raw);
        files.push({ ref, path: absPath, content, truncated });
      } catch {
        errors.push(`@${ref}: could not read`);
      }
    }

    if (files.length === 0 && errors.length === 0) return { action: "continue" as const };

    // Replace @references in the user's message with absolute paths
    // so the model sees the real path, not the ambiguous shorthand
    let transformedText = text;
    for (const f of files) {
      transformedText = transformedText.replace(`@${f.ref}`, f.path);
    }

    setSegment("at-file", `@${files.length}`, { order: 40 });

    // Inject file contents as non-displayed followUp — model sees it,
    // user doesn't scroll through file dumps
    if (files.length > 0) {
      const fileBlocks = files.map(f =>
        `<file path="${f.path}">\n${f.content}\n</file>`
      ).join("\n\n");

      pi.sendMessage(
        {
          customType: "at-file",
          content: `Contents of referenced files (already loaded — do NOT re-read these):\n\n${fileBlocks}`,
          display: false,
        },
        { deliverAs: "followUp" },
      );

      // Show user which files were included
      const summary = files.map(f => {
        const lines = f.content.split("\n").length;
        const suffix = f.truncated ? " (truncated)" : "";
        return `  ${f.path} (${lines} lines${suffix})`;
      }).join("\n");

      pi.sendMessage(
        {
          customType: "at-file-summary",
          content: `Included files:\n${summary}`,
          display: true,
        },
        { deliverAs: "followUp" },
      );
    }

    if (errors.length > 0) {
      pi.sendMessage({
        customType: "at-file-error",
        content: errors.join("\n"),
        display: true,
      });
    }

    // Transform the user's message: @ref → absolute path
    return { action: "transform" as const, text: transformedText };
  });
}
