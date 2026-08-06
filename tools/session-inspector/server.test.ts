import { afterEach, describe, expect, test } from "bun:test";
import { appendFile, mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { analyzeSession, createInspector, resolveDataDir } from "./server";

const roots: string[] = [];

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true })));
});

const fixture = async () => {
  const root = await mkdtemp(join(tmpdir(), "nefor-session-inspector-"));
  roots.push(root);
  const dataDir = join(root, "data");
  const sessionsDir = join(dataDir, "sessions");
  const cacheDir = join(root, "cache");
  await mkdir(sessionsDir, { recursive: true });
  const envelope = (body: unknown) =>
    JSON.stringify({ origin: "fixture", ts: "1", payload: JSON.stringify({ type: "event", from: "fixture", ts: "1", body }) });
  const message = { role: "user", content: "same message" };
  const lines = [
    envelope({ kind: "chat.history.message", provider: "chatgpt", chat_id: "c1", message }),
    envelope({ kind: "chatgpt.chat.append", chat_id: "c1", message }),
    envelope({ kind: "mag.node_preview", run_id: "r1", id: "lead", value: "preview" }),
  ];
  await writeFile(join(sessionsDir, "session-one.jsonl"), `${lines.join("\n")}\n`);
  return { root, dataDir, sessionsDir, cacheDir };
};

describe("session inspector", () => {
  test("resolves the same data-directory precedence as Nefor", () => {
    expect(resolveDataDir({ NEFOR_DATA_DIR: "/explicit", XDG_DATA_HOME: "/xdg", HOME: "/home" })).toBe("/explicit");
    expect(resolveDataDir({ XDG_DATA_HOME: "/xdg", HOME: "/home" })).toBe("/xdg/nefor");
    expect(resolveDataDir({ HOME: "/home" })).toBe("/home/.local/share/nefor");
  });

  test("streams structural summaries without retaining message contents", async () => {
    const { sessionsDir } = await fixture();
    const job = { id: "job", sessionId: "session-one", status: "running" as const, bytesRead: 0, totalBytes: 0, rows: 0 };
    const summary = await analyzeSession(sessionsDir, "session-one", job);
    expect(summary.rows).toBe(3);
    expect(summary.kinds[0].rawBytes).toBeGreaterThan(0);
    expect(summary.pairing[0]).toMatchObject({ matched: 1, leftUnique: 1, rightUnique: 1 });
    expect(JSON.stringify(summary)).not.toContain("same message");
  });

  test("lists sessions and completes an analysis job through the HTTP surface", async () => {
    const { dataDir, sessionsDir, cacheDir } = await fixture();
    const inspector = createInspector({ dataDir, cacheDir });
    const sessions = await inspector.fetch(new Request("http://local/api/sessions"));
    expect(await sessions.json()).toMatchObject({ sessions: [{ id: "session-one" }] });

    const started = await inspector.fetch(new Request("http://local/api/sessions/session-one/analyze", { method: "POST" }));
    const startedBody = await started.json() as { job: { id: string } };
    let job: { status: string; summary?: { rows: number } } | undefined;
    for (let attempt = 0; attempt < 50; attempt += 1) {
      const response = await inspector.fetch(new Request(`http://local/api/jobs/${startedBody.job.id}`));
      job = (await response.json() as { job: typeof job }).job;
      if (job?.status === "complete") break;
      await Bun.sleep(5);
    }
    expect(job).toMatchObject({ status: "complete", summary: { rows: 3 } });

    await appendFile(
      join(sessionsDir, "session-one.jsonl"),
      `${JSON.stringify({ payload: JSON.stringify({ body: { kind: "tool.result" } }) })}\n`,
    );
    const restarted = await inspector.fetch(new Request("http://local/api/sessions/session-one/analyze", { method: "POST" }));
    const restartedBody = await restarted.json() as { job: { id: string } };
    expect(restartedBody.job.id).not.toBe(startedBody.job.id);

    for (let attempt = 0; attempt < 50; attempt += 1) {
      const response = await inspector.fetch(new Request(`http://local/api/jobs/${restartedBody.job.id}`));
      job = (await response.json() as { job: typeof job }).job;
      if (job?.status === "complete") break;
      await Bun.sleep(5);
    }
    expect(job).toMatchObject({ status: "complete", summary: { rows: 4 } });
  });
});
