#!/usr/bin/env bun

import { existsSync, lstatSync, readFileSync, readdirSync } from "node:fs";
import { dirname, extname, join, normalize, relative, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const excluded = new Set([".git", "target", "tmp"]);

function markdownFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    if (excluded.has(entry.name)) return [];
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return markdownFiles(path);
    return extname(entry.name) === ".md" ? [path] : [];
  });
}

function slugify(heading: string): string {
  return heading
    .trim()
    .toLowerCase()
    .replace(/<[^>]*>/g, "")
    .replace(/[`*_~]/g, "")
    .replace(/[^\p{L}\p{N}\s-]/gu, "")
    .replace(/\s/g, "-");
}

function anchors(markdown: string): Set<string> {
  const seen = new Map<string, number>();
  const result = new Set<string>();
  for (const line of markdown.split("\n")) {
    const match = /^(#{1,6})\s+(.+?)\s*#*\s*$/.exec(line);
    if (!match) continue;
    const base = slugify(match[2]);
    const count = seen.get(base) ?? 0;
    seen.set(base, count + 1);
    result.add(count === 0 ? base : `${base}-${count}`);
  }
  return result;
}

const errors: string[] = [];
for (const file of markdownFiles(root)) {
  const markdown = readFileSync(file, "utf8");
  const withoutFences = markdown.replace(/^```[\s\S]*?^```\s*$/gm, "");
  const links = withoutFences.matchAll(/(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+["'][^"']*["'])?\)/g);
  for (const match of links) {
    const destination = match[1];
    if (/^(?:https?:|mailto:)/i.test(destination)) continue;
    const [encodedPath, encodedFragment] = destination.split("#", 2);
    let targetPath: string;
    try {
      targetPath = encodedPath
        ? normalize(resolve(dirname(file), decodeURIComponent(encodedPath)))
        : file;
    } catch {
      errors.push(`${relative(root, file)}: malformed URL encoding in ${destination}`);
      continue;
    }
    if (!existsSync(targetPath)) {
      errors.push(`${relative(root, file)}: missing target ${destination}`);
      continue;
    }
    if (lstatSync(targetPath).isDirectory()) continue;
    const target = targetPath;
    if (encodedFragment && extname(target) === ".md") {
      const fragment = decodeURIComponent(encodedFragment).toLowerCase();
      if (!anchors(readFileSync(target, "utf8")).has(fragment)) {
        errors.push(`${relative(root, file)}: missing anchor #${encodedFragment} in ${relative(root, target)}`);
      }
    }
  }
}

if (errors.length > 0) {
  console.error(errors.join("\n"));
  process.exit(1);
}
console.log("Markdown links and anchors are valid.");
