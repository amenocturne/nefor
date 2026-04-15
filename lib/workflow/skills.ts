import { existsSync, readFileSync, readdirSync, statSync } from "fs";
import { spawn } from "child_process";
import { join, resolve } from "path";
import { parse } from "yaml";
import type { BackgroundSkillHandle, SkillEntry, SkillRegistry, SkillResult } from "./types.ts";

// ── Helpers ─────────────────────────────────────────────────────────────

const shellEscape = (arg: string): string =>
  `'${arg.replace(/'/g, "'\\''")}'`;

// ── Discovery ───────────────────────────────────────────────────────────

export function discoverSkills(skillsDir: string): SkillRegistry {
  const registry: SkillRegistry = new Map();

  if (!existsSync(skillsDir)) return registry;

  const entries = readdirSync(skillsDir);

  for (const entry of entries) {
    const dirPath = resolve(join(skillsDir, entry));
    if (!statSync(dirPath).isDirectory()) continue;

    const skillMdPath = join(dirPath, "SKILL.md");
    if (!existsSync(skillMdPath)) continue;

    const content = readFileSync(skillMdPath, "utf-8");
    const match = content.match(/^---\n([\s\S]*?)\n---/);
    if (!match) continue;

    const frontmatter = parse(match[1]);
    if (!frontmatter?.name || !frontmatter?.run) continue;

    const command = frontmatter.run.replace(/\{skill_path\}/g, dirPath);

    registry.set(frontmatter.name, {
      name: frontmatter.name,
      command,
      skillPath: dirPath,
    });
  }

  return registry;
}

// ── Execution ───────────────────────────────────────────────────────────

export function runSkill(entry: SkillEntry, args: string[]): Promise<SkillResult> {
  return new Promise((resolve) => {
    const fullCommand = buildCommand(entry, args);

    const proc = spawn("sh", ["-c", fullCommand], {
      cwd: entry.skillPath,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env },
    });

    let stdout = "";
    proc.stdout!.setEncoding("utf-8");
    proc.stdout!.on("data", (chunk: string) => { stdout += chunk; });

    proc.stderr!.resume();

    proc.on("close", (code) => {
      resolve({ stdout, exitCode: code ?? 1 });
    });

    proc.on("error", () => {
      resolve({ stdout, exitCode: 1 });
    });
  });
}

export function runSkillBackground(entry: SkillEntry, args: string[]): BackgroundSkillHandle {
  const fullCommand = buildCommand(entry, args);

  const proc = spawn("sh", ["-c", fullCommand], {
    cwd: entry.skillPath,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env },
  });

  let stdout = "";
  let stderr = "";

  proc.stdout!.setEncoding("utf-8");
  proc.stdout!.on("data", (chunk: string) => { stdout += chunk; });

  proc.stderr!.setEncoding("utf-8");
  proc.stderr!.on("data", (chunk: string) => { stderr += chunk; });

  const done = new Promise<SkillResult>((resolve) => {
    proc.on("close", (code) => {
      resolve({ stdout, exitCode: code ?? 1 });
    });
    proc.on("error", () => {
      resolve({ stdout, exitCode: 1 });
    });
  });

  return {
    stdout: () => stdout,
    stderr: () => stderr,
    done,
  };
}

// ── Internal ───────────────────────────────────────────────────────────

const buildCommand = (entry: SkillEntry, args: string[]): string =>
  args.length > 0
    ? `${entry.command} ${args.map(a => shellEscape(a)).join(" ")}`
    : entry.command;
