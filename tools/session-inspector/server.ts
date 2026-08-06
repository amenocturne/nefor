import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { mkdir, readdir, readFile, rename, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { createInterface } from "node:readline";

const CACHE_VERSION = 1;
const SESSION_ID = /^[a-zA-Z0-9][a-zA-Z0-9_-]*$/;

type CountedBytes = { rows: number; rawBytes: number };
type FieldSummary = { field: string; rows: number; bytes: number };
type KindSummary = CountedBytes & {
  kind: string;
  bodyBytes: number;
  fields: FieldSummary[];
};
type PairSummary = {
  label: string;
  left: string;
  right: string;
  leftCount: number;
  rightCount: number;
  leftUnique: number;
  rightUnique: number;
  matched: number;
  matchedBytesEachSide: number;
};
export type SessionSummary = {
  sessionId: string;
  source: { bytes: number; mtimeMs: number };
  rows: number;
  invalidOuterRows: number;
  invalidPayloadRows: number;
  categories: Array<CountedBytes & { category: string }>;
  kinds: KindSummary[];
  pairing: PairSummary[];
  largest: Array<{ line: number; kind: string; bytes: number; fields: string[] }>;
  elapsedMs: number;
};
type Job = {
  id: string;
  sessionId: string;
  status: "queued" | "running" | "complete" | "error";
  bytesRead: number;
  totalBytes: number;
  rows: number;
  error?: string;
  summary?: SessionSummary;
};
type MutableKind = CountedBytes & {
  bodyBytes: number;
  fields: Map<string, { rows: number; bytes: number }>;
};
type DigestCount = { count: number; bytes: number };
type PairAccumulator = {
  label: string;
  leftKind: string;
  leftField: string;
  rightKind: string;
  rightField: string;
  left: Map<string, DigestCount>;
  right: Map<string, DigestCount>;
};

export type InspectorOptions = {
  dataDir: string;
  cacheDir: string;
  staticDir?: string;
};

const json = (value: unknown, status = 200): Response =>
  Response.json(value, {
    status,
    headers: { "cache-control": "no-store" },
  });

const byteLength = (value: unknown): number =>
  Buffer.byteLength(JSON.stringify(value));

const categoryOf = (kind: string): string => {
  if (kind === "mag.arrival") return "MAG canonical values";
  if (kind === "mag.firing") return "MAG firing references";
  if (kind === "mag.node_preview") return "legacy node previews";
  if (/^(chat\.history|chat\.message|chat\.input)/.test(kind)) {
    return "conversation projection";
  }
  if (/^(chatgpt\.chat|openai\.chat)/.test(kind)) return "provider chat state";
  if (kind === "tool.stream") return "tool streaming";
  if (kind.startsWith("tool.")) return "tool lifecycle";
  if (kind.startsWith("mag.")) return "MAG workflow";
  if (kind.startsWith("conversation.")) return "conversation manager";
  return "other";
};

const digestValue = (value: unknown): { digest: string; bytes: number } => {
  const encoded = JSON.stringify(value);
  return {
    digest: createHash("sha256").update(encoded).digest("hex"),
    bytes: Buffer.byteLength(encoded),
  };
};

const incrementDigest = (
  destination: Map<string, DigestCount>,
  value: unknown,
): void => {
  const { digest, bytes } = digestValue(value);
  const current = destination.get(digest);
  destination.set(digest, { count: (current?.count ?? 0) + 1, bytes });
};

const summarizePair = (pair: PairAccumulator): PairSummary => {
  let matched = 0;
  let matchedBytesEachSide = 0;
  for (const [digest, left] of pair.left) {
    const right = pair.right.get(digest);
    if (!right) continue;
    const occurrences = Math.min(left.count, right.count);
    matched += occurrences;
    matchedBytesEachSide += occurrences * left.bytes;
  }
  return {
    label: pair.label,
    left: `${pair.leftKind}.${pair.leftField}`,
    right: `${pair.rightKind}.${pair.rightField}`,
    leftCount: [...pair.left.values()].reduce((sum, item) => sum + item.count, 0),
    rightCount: [...pair.right.values()].reduce((sum, item) => sum + item.count, 0),
    leftUnique: pair.left.size,
    rightUnique: pair.right.size,
    matched,
    matchedBytesEachSide,
  };
};

const cachePath = (cacheDir: string, sessionId: string): string =>
  join(cacheDir, `${sessionId}.json`);

const readCached = async (
  cacheDir: string,
  sessionId: string,
  source: { bytes: number; mtimeMs: number },
): Promise<SessionSummary | undefined> => {
  try {
    const parsed = JSON.parse(await readFile(cachePath(cacheDir, sessionId), "utf8"));
    if (
      parsed.version === CACHE_VERSION &&
      parsed.source?.bytes === source.bytes &&
      parsed.source?.mtimeMs === source.mtimeMs
    ) {
      return parsed.summary as SessionSummary;
    }
  } catch {
    return undefined;
  }
  return undefined;
};

const writeCached = async (
  cacheDir: string,
  sessionId: string,
  summary: SessionSummary,
): Promise<void> => {
  await mkdir(cacheDir, { recursive: true });
  const destination = cachePath(cacheDir, sessionId);
  const temporary = `${destination}.${randomUUID()}.tmp`;
  await writeFile(
    temporary,
    JSON.stringify({ version: CACHE_VERSION, source: summary.source, summary }),
  );
  await rename(temporary, destination);
};

export const analyzeSession = async (
  sessionsDir: string,
  sessionId: string,
  job: Job,
): Promise<SessionSummary> => {
  const path = join(sessionsDir, `${sessionId}.jsonl`);
  const sourceStat = await stat(path);
  const source = { bytes: sourceStat.size, mtimeMs: sourceStat.mtimeMs };
  const started = performance.now();
  const kinds = new Map<string, MutableKind>();
  const categories = new Map<string, CountedBytes>();
  const largest: SessionSummary["largest"] = [];
  const pairs: PairAccumulator[] = [
    {
      label: "messages",
      leftKind: "chat.history.message",
      leftField: "message",
      rightKind: "chatgpt.chat.append",
      rightField: "message",
      left: new Map(),
      right: new Map(),
    },
    {
      label: "system prompts",
      leftKind: "chat.history.create",
      leftField: "system",
      rightKind: "chatgpt.chat.create",
      rightField: "system",
      left: new Map(),
      right: new Map(),
    },
  ];
  let rows = 0;
  let invalidOuterRows = 0;
  let invalidPayloadRows = 0;

  if (source.bytes === 0) {
    return {
      sessionId,
      source,
      rows,
      invalidOuterRows,
      invalidPayloadRows,
      categories: [],
      kinds: [],
      pairing: pairs.map(summarizePair),
      largest,
      elapsedMs: performance.now() - started,
    };
  }

  const stream = createReadStream(path, {
    end: source.bytes - 1,
    highWaterMark: 4 * 1024 * 1024,
  });
  stream.on("data", (chunk) => {
    job.bytesRead += chunk.length;
  });
  const lines = createInterface({ input: stream, crlfDelay: Infinity });

  for await (const line of lines) {
    rows += 1;
    job.rows = rows;
    const rawBytes = Buffer.byteLength(line) + 1;
    let outer: unknown;
    try {
      outer = JSON.parse(line);
    } catch {
      invalidOuterRows += 1;
      continue;
    }
    const payload =
      typeof outer === "object" && outer !== null && "payload" in outer
        ? (outer as { payload?: unknown }).payload
        : undefined;
    let body: Record<string, unknown> | undefined;
    if (typeof payload === "string") {
      try {
        const inner = JSON.parse(payload);
        if (typeof inner?.body === "object" && inner.body !== null) {
          body = inner.body as Record<string, unknown>;
        }
      } catch {
        invalidPayloadRows += 1;
      }
    }
    const kind = typeof body?.kind === "string" ? body.kind : "<missing-kind>";
    const currentKind = kinds.get(kind) ?? {
      rows: 0,
      rawBytes: 0,
      bodyBytes: 0,
      fields: new Map(),
    };
    currentKind.rows += 1;
    currentKind.rawBytes += rawBytes;
    if (body) {
      currentKind.bodyBytes += byteLength(body);
      for (const [field, value] of Object.entries(body)) {
        const current = currentKind.fields.get(field) ?? { rows: 0, bytes: 0 };
        current.rows += 1;
        current.bytes += Math.max(0, byteLength({ [field]: value }) - 2);
        currentKind.fields.set(field, current);
      }
    }
    kinds.set(kind, currentKind);

    const category = categoryOf(kind);
    const currentCategory = categories.get(category) ?? { rows: 0, rawBytes: 0 };
    currentCategory.rows += 1;
    currentCategory.rawBytes += rawBytes;
    categories.set(category, currentCategory);

    if (body) {
      for (const pair of pairs) {
        if (kind === pair.leftKind && body[pair.leftField] !== undefined) {
          incrementDigest(pair.left, body[pair.leftField]);
        } else if (kind === pair.rightKind && body[pair.rightField] !== undefined) {
          incrementDigest(pair.right, body[pair.rightField]);
        }
      }
    }

    largest.push({
      line: rows,
      kind,
      bytes: rawBytes,
      fields: body ? Object.keys(body).sort() : [],
    });
    largest.sort((left, right) => right.bytes - left.bytes);
    if (largest.length > 20) largest.pop();
  }

  return {
    sessionId,
    source,
    rows,
    invalidOuterRows,
    invalidPayloadRows,
    categories: [...categories]
      .map(([category, value]) => ({ category, ...value }))
      .sort((left, right) => right.rawBytes - left.rawBytes),
    kinds: [...kinds]
      .map(([kind, value]) => ({
        kind,
        rows: value.rows,
        rawBytes: value.rawBytes,
        bodyBytes: value.bodyBytes,
        fields: [...value.fields]
          .map(([field, fieldValue]) => ({ field, ...fieldValue }))
          .sort((left, right) => right.bytes - left.bytes)
          .slice(0, 30),
      }))
      .sort((left, right) => right.rawBytes - left.rawBytes),
    pairing: pairs.map(summarizePair),
    largest,
    elapsedMs: performance.now() - started,
  };
};

const resolveStatic = (staticDir: string, pathname: string): string | undefined => {
  if (pathname === "/") return join(staticDir, "index.html");
  if (pathname === "/app.js") return join(staticDir, "app.js");
  if (pathname === "/styles.css") return join(staticDir, "styles.css");
  return undefined;
};

const contentType = (path: string): string => {
  if (path.endsWith(".html")) return "text/html; charset=utf-8";
  if (path.endsWith(".js")) return "text/javascript; charset=utf-8";
  return "text/css; charset=utf-8";
};

export const createInspector = (options: InspectorOptions) => {
  const sessionsDir = join(options.dataDir, "sessions");
  const staticDir = options.staticDir ?? new URL(".", import.meta.url).pathname;
  const jobs = new Map<string, Job>();
  const currentBySession = new Map<string, string>();
  let queue = Promise.resolve();

  const listSessions = async () => {
    const entries = await readdir(sessionsDir, { withFileTypes: true }).catch(() => []);
    const sessions = await Promise.all(
      entries
        .filter((entry) => {
          if (!entry.isFile() || !entry.name.endsWith(".jsonl")) return false;
          return SESSION_ID.test(basename(entry.name, ".jsonl"));
        })
        .map(async (entry) => {
          const path = join(sessionsDir, entry.name);
          const fileStat = await stat(path);
          return {
            id: basename(entry.name, ".jsonl"),
            bytes: fileStat.size,
            mtimeMs: fileStat.mtimeMs,
          };
        }),
    );
    return sessions.sort((left, right) => right.mtimeMs - left.mtimeMs);
  };

  const startJob = async (sessionId: string): Promise<Job> => {
    if (!SESSION_ID.test(sessionId)) throw new Error("invalid session id");
    const existingId = currentBySession.get(sessionId);
    const existing = existingId ? jobs.get(existingId) : undefined;
    if (existing && (existing.status === "queued" || existing.status === "running")) {
      return existing;
    }
    const fileStat = await stat(join(sessionsDir, `${sessionId}.jsonl`));
    const source = { bytes: fileStat.size, mtimeMs: fileStat.mtimeMs };
    const job: Job = {
      id: randomUUID(),
      sessionId,
      status: "queued",
      bytesRead: 0,
      totalBytes: source.bytes,
      rows: 0,
    };
    jobs.set(job.id, job);
    currentBySession.set(sessionId, job.id);
    queue = queue
      .catch(() => undefined)
      .then(async () => {
        job.status = "running";
        try {
          const cached = await readCached(options.cacheDir, sessionId, source);
          job.summary = cached ?? (await analyzeSession(sessionsDir, sessionId, job));
          job.bytesRead = job.totalBytes;
          job.rows = job.summary.rows;
          if (!cached) await writeCached(options.cacheDir, sessionId, job.summary);
          job.status = "complete";
        } catch (error) {
          job.status = "error";
          job.error = error instanceof Error ? error.message : String(error);
        }
      });
    return job;
  };

  const fetch = async (request: Request): Promise<Response> => {
    const url = new URL(request.url);
    if (url.pathname === "/api/sessions" && request.method === "GET") {
      return json({ sessions: await listSessions(), dataDir: options.dataDir });
    }
    const analyzeMatch = url.pathname.match(/^\/api\/sessions\/([^/]+)\/analyze$/);
    if (analyzeMatch && request.method === "POST") {
      try {
        return json({ job: await startJob(decodeURIComponent(analyzeMatch[1])) }, 202);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        return json({ error: message }, message === "invalid session id" ? 400 : 404);
      }
    }
    const jobMatch = url.pathname.match(/^\/api\/jobs\/([^/]+)$/);
    if (jobMatch && request.method === "GET") {
      const job = jobs.get(jobMatch[1]);
      return job ? json({ job }) : json({ error: "job not found" }, 404);
    }
    const staticPath = resolveStatic(staticDir, url.pathname);
    if (staticPath && request.method === "GET") {
      const file = Bun.file(staticPath);
      if (await file.exists()) {
        return new Response(file, {
          headers: {
            "content-type": contentType(staticPath),
            "content-security-policy":
              "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
            "x-content-type-options": "nosniff",
          },
        });
      }
    }
    return json({ error: "not found" }, 404);
  };

  return { fetch, listSessions, startJob };
};

export const resolveDataDir = (environment: NodeJS.ProcessEnv = process.env): string => {
  if (environment.NEFOR_DATA_DIR) return environment.NEFOR_DATA_DIR;
  if (environment.XDG_DATA_HOME) return join(environment.XDG_DATA_HOME, "nefor");
  return join(environment.HOME || homedir(), ".local", "share", "nefor");
};

export const resolveCacheDir = (environment: NodeJS.ProcessEnv = process.env): string => {
  if (environment.XDG_CACHE_HOME) {
    return join(environment.XDG_CACHE_HOME, "nefor", "session-inspector");
  }
  return join(environment.HOME || homedir(), ".cache", "nefor", "session-inspector");
};

if (import.meta.main) {
  const hostname = process.env.NEFOR_SESSION_INSPECTOR_HOST || "127.0.0.1";
  const port = Number(process.env.NEFOR_SESSION_INSPECTOR_PORT || "3939");
  const inspector = createInspector({
    dataDir: resolveDataDir(),
    cacheDir: resolveCacheDir(),
  });
  const server = Bun.serve({ hostname, port, fetch: inspector.fetch });
  console.log(`Nefor session inspector: ${server.url}`);
  console.log(`Data directory: ${resolveDataDir()}`);
}
