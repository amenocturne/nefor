/**
 * Model Performance — Pi extension for real-time throughput monitoring
 *
 * Shows tokens/sec in the status bar during streaming. Tracks both
 * text output and thinking tokens separately. Displays a summary
 * after each message completes.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { setSegment } from "../statusline/index.ts";

const ORDER = 30;

export default function (pi: ExtensionAPI) {
  // Per-message streaming state
  let messageStartTime = 0;
  let textTokens = 0;
  let thinkTokens = 0;
  let lastUpdateTime = 0;
  let recentTokenTimes: number[] = [];

  // Session totals
  let sessionTextTokens = 0;
  let sessionThinkTokens = 0;
  let sessionMessages = 0;

  const ROLLING_WINDOW_MS = 3_000;
  const STATUS_KEY = "model-perf";

  const rollingRate = (now: number): number => {
    const cutoff = now - ROLLING_WINDOW_MS;
    recentTokenTimes = recentTokenTimes.filter(t => t > cutoff);
    if (recentTokenTimes.length < 2) return 0;
    const span = (now - recentTokenTimes[0]) / 1000;
    return span > 0 ? recentTokenTimes.length / span : 0;
  };

  const formatRate = (rate: number): string =>
    rate >= 100 ? `${Math.round(rate)}` : `${rate.toFixed(1)}`;

  pi.on("session_start", async () => {
    sessionTextTokens = 0;
    sessionThinkTokens = 0;
    sessionMessages = 0;
  });

  pi.on("message_start", async (event) => {
    if ((event.message as any).role !== "assistant") return;
    messageStartTime = Date.now();
    textTokens = 0;
    thinkTokens = 0;
    recentTokenTimes = [];
    lastUpdateTime = 0;
  });

  pi.on("message_update", async (event, ctx) => {
    if (!messageStartTime) return;
    const ame = event.assistantMessageEvent;
    const now = Date.now();

    if (ame.type === "text_delta") {
      textTokens++;
      recentTokenTimes.push(now);
    } else if (ame.type === "thinking_delta") {
      thinkTokens++;
      recentTokenTimes.push(now);
    } else if (ame.type === "toolcall_delta") {
      textTokens++;
      recentTokenTimes.push(now);
    } else {
      return;
    }

    if (now - lastUpdateTime > 200) {
      lastUpdateTime = now;
      const rate = rollingRate(now);
      setSegment(STATUS_KEY, `${formatRate(rate)} tok/s`, { order: ORDER, color: "success" });
    }
  });

  pi.on("message_end", async (event, ctx) => {
    if (!messageStartTime) return;
    if ((event.message as any).role !== "assistant") return;

    const elapsed = (Date.now() - messageStartTime) / 1000;
    const totalTokens = textTokens + thinkTokens;
    const avgRate = elapsed > 0 ? totalTokens / elapsed : 0;

    sessionTextTokens += textTokens;
    sessionThinkTokens += thinkTokens;
    sessionMessages++;

    const sessionTotal = sessionTextTokens + sessionThinkTokens;
    const sessionStr = sessionTotal >= 1000
      ? `${(sessionTotal / 1000).toFixed(1)}k`
      : `${sessionTotal}`;

    const parts = [`${formatRate(avgRate)} tok/s`, `${elapsed.toFixed(1)}s`, `Σ${sessionStr}`];
    setSegment(STATUS_KEY, parts.join(" · "), { order: ORDER, color: "dim" });
    messageStartTime = 0;
  });
}
