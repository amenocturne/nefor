/**
 * AGENTS.md Includes — expand @file references, parse validation_commands,
 * and auto-inject AGENTS.md when agents access files in subdirectories.
 *
 * Three responsibilities:
 * 1. On before_agent_start: expand @path refs in AGENTS.md system prompt sections
 * 2. On before_agent_start: parse validation_commands from AGENTS.md frontmatter
 * 3. On tool_call: when agent reads/writes files in a subdirectory that has an
 *    AGENTS.md between the file and the agent's cwd, inject it before the tool runs
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { existsSync, readFileSync } from "fs";
import { resolve, dirname, join, relative } from "path";

// Match @path references in AGENTS.md content
const AT_FILE_RE = /(?:^|(?<=\s))@((?:\.\/|\.\.\/)[^\s,]+|[^\s,]+\.[^\s,]+)/gm;

// Match the AGENTS.md section header injected by Pi
const AGENTS_SECTION_RE = /^## (\/[^\n]+\/AGENTS\.md)\s*$/gm;

// Match YAML frontmatter
const FRONTMATTER_RE = /^---\n([\s\S]*?)\n---/;

// Tools that access file paths
const FILE_TOOLS: Record<string, string> = {
  read: "path",
  write: "path",
  edit: "path",
  grep: "path",
  find: "path",
};

type ValidationConfig = {
  dir: string;
  commands: string[];
};

let validationConfigs: ValidationConfig[] = [];

export function getValidationConfigs(): ValidationConfig[] {
  return validationConfigs;
}

function parseFrontmatter(content: string): { commands: string[]; bodyWithoutFrontmatter: string } {
  const match = content.match(FRONTMATTER_RE);
  if (!match) return { commands: [], bodyWithoutFrontmatter: content };

  const yaml = match[1];
  const bodyWithoutFrontmatter = content.slice(match[0].length).trimStart();

  const commandsMatch = yaml.match(/validation_commands:\s*\n((?:\s+-\s+.+\n?)*)/);
  if (!commandsMatch) return { commands: [], bodyWithoutFrontmatter };

  const commands = commandsMatch[1]
    .split("\n")
    .map(line => line.replace(/^\s+-\s+/, "").trim())
    .filter(Boolean);

  return { commands, bodyWithoutFrontmatter };
}

function expandAtFiles(content: string, baseDir: string): string {
  const matches = [...content.matchAll(AT_FILE_RE)];
  if (matches.length === 0) return content;

  const appendedFiles: string[] = [];
  const errors: string[] = [];

  for (const match of matches) {
    const ref = match[1];
    const absPath = resolve(baseDir, ref);

    if (!existsSync(absPath)) {
      errors.push(`@${ref}: file not found (resolved to ${absPath})`);
      continue;
    }

    try {
      const fileContent = readFileSync(absPath, "utf-8");
      appendedFiles.push(`### @${ref}\n\n${fileContent}`);
    } catch {
      errors.push(`@${ref}: could not read`);
    }
  }

  if (appendedFiles.length === 0 && errors.length === 0) return content;

  let result = content;

  if (appendedFiles.length > 0) {
    result += "\n\n---\n\n" + appendedFiles.join("\n\n---\n\n");
  }

  if (errors.length > 0) {
    result += "\n\n<!-- agents-md-includes errors:\n" + errors.join("\n") + "\n-->";
  }

  return result;
}

/**
 * Walk from a file's directory up to (but not including) cwd, collecting
 * AGENTS.md files that exist in intermediate directories. These are the
 * ones Pi missed because it only walks UP from cwd, not DOWN into subdirs.
 */
function findIntermediateAgentsMd(filePath: string, cwd: string): string[] {
  const resolvedCwd = resolve(cwd);
  const resolvedFile = resolve(cwd, filePath);
  const fileDir = dirname(resolvedFile);

  // Only look at paths below cwd
  const rel = relative(resolvedCwd, fileDir);
  if (!rel || rel.startsWith("..") || rel.startsWith("/")) return [];

  const found: string[] = [];
  let current = fileDir;

  while (current !== resolvedCwd && current.startsWith(resolvedCwd + "/")) {
    const agentsMd = join(current, "AGENTS.md");
    if (existsSync(agentsMd)) {
      found.unshift(agentsMd); // parent first, child last
    }
    current = dirname(current);
  }

  return found;
}

function loadAndExpandAgentsMd(path: string): string {
  const raw = readFileSync(path, "utf-8");
  const baseDir = dirname(path);

  // Strip frontmatter (validation_commands etc.) — not relevant for injection
  const { bodyWithoutFrontmatter } = parseFrontmatter(raw);
  const body = bodyWithoutFrontmatter || raw;

  // Expand @file references
  return expandAtFiles(body, baseDir);
}

export default function (pi: ExtensionAPI) {
  let cwd = ".";
  const injectedAgentsMd = new Set<string>();

  pi.on("session_start", async (_event, ctx) => {
    cwd = ctx.cwd;
  });

  // ── System prompt processing ────────────────────────────────────────

  pi.on("before_agent_start", async (event) => {
    const systemPrompt = event.systemPrompt;
    if (!systemPrompt) return;

    let modified = systemPrompt;
    let hasChanges = false;
    validationConfigs = [];

    const sectionStarts: Array<{ index: number; path: string }> = [];
    let sectionMatch;

    AGENTS_SECTION_RE.lastIndex = 0;
    while ((sectionMatch = AGENTS_SECTION_RE.exec(systemPrompt)) !== null) {
      sectionStarts.push({ index: sectionMatch.index, path: sectionMatch[1] });
    }

    if (sectionStarts.length === 0) return;

    // Track AGENTS.md files loaded at startup so we don't re-inject them
    for (const section of sectionStarts) {
      injectedAgentsMd.add(section.path);
    }

    for (let i = sectionStarts.length - 1; i >= 0; i--) {
      const section = sectionStarts[i];
      const nextStart = i < sectionStarts.length - 1
        ? sectionStarts[i + 1].index
        : undefined;

      const sectionContent = nextStart
        ? modified.slice(section.index, nextStart)
        : modified.slice(section.index);

      const baseDir = dirname(section.path);

      const headerEnd = sectionContent.indexOf("\n") + 1;
      const bodyContent = sectionContent.slice(headerEnd);
      const { commands, bodyWithoutFrontmatter } = parseFrontmatter(bodyContent);

      if (commands.length > 0) {
        validationConfigs.push({ dir: baseDir, commands });
      }

      const expanded = expandAtFiles(
        commands.length > 0 ? bodyWithoutFrontmatter : bodyContent,
        baseDir,
      );

      if (expanded !== bodyContent || commands.length > 0) {
        const newSection = sectionContent.slice(0, headerEnd) + expanded;
        modified = nextStart
          ? modified.slice(0, section.index) + newSection + modified.slice(nextStart)
          : modified.slice(0, section.index) + newSection;
        hasChanges = true;
      }
    }

    if (hasChanges) {
      return { systemPrompt: modified };
    }
  });

  // ── Runtime AGENTS.md injection on file access ──────────────────────

  pi.on("tool_call", async (event) => {
    const pathParam = FILE_TOOLS[event.toolName];
    if (!pathParam) return { block: false };

    const input = event.input as Record<string, unknown>;
    const filePath = (input[pathParam] ?? input.file_path ?? "") as string;
    if (!filePath) return { block: false };

    const intermediates = findIntermediateAgentsMd(filePath, cwd);
    const toInject = intermediates.filter(p => !injectedAgentsMd.has(p));

    if (toInject.length === 0) return { block: false };

    // Inject each AGENTS.md as a system directive before the tool executes
    for (const agentsMdPath of toInject) {
      injectedAgentsMd.add(agentsMdPath);
      try {
        const content = loadAndExpandAgentsMd(agentsMdPath);
        pi.sendMessage(
          {
            customType: "agents-md-inject",
            content: `## ${agentsMdPath}\n\n${content}`,
            display: false,
          },
          { deliverAs: "steer" },
        );
      } catch {
        // Silently skip unreadable files
      }
    }

    return { block: false };
  });
}
