/**
 * Nestor DP Auth Provider Extension for Pi
 *
 * Connects Pi to Tinkoff's internal LLM API via DP (DevPlatform) authentication.
 * The Nestor API is OpenAI-compatible, so we delegate streaming to Pi's built-in
 * OpenAI Completions implementation with a custom Nestor-Token header.
 *
 * Usage:
 *   pi -e ./path/to/pi-nestor-provider
 *   /login nestor          # triggers DP auth (browser SSO)
 *
 *   # Or if you already have a DP session:
 *   pi -e ./path/to/pi-nestor-provider --provider nestor
 */

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import {
	type Api,
	type AssistantMessageEventStream,
	type Context,
	createAssistantMessageEventStream,
	type Model,
	type OAuthCredentials,
	type OAuthLoginCallbacks,
	type SimpleStreamOptions,
	streamSimpleOpenAICompletions,
} from "@mariozechner/pi-ai";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

// =============================================================================
// Constants
// =============================================================================

const NESTOR_BASE = "https://code-completion-nestor.tcsbank.ru";
const API_BASE = `${NESTOR_BASE}/api/v1/cli/openai-like/v1`;
const TOKEN_ENDPOINT = `${NESTOR_BASE}/api/v2/token`;
const MODELS_ENDPOINT = `${NESTOR_BASE}/api/v1/cli/models`;

const DP_WORKDIR_NAME = "dp_v13.4.2";

// =============================================================================
// DP Binary
// =============================================================================

function findDpBinary(): string {
	const candidates = [
		"/usr/local/bin/dp",
		join(homedir(), ".nessy", DP_WORKDIR_NAME, "dp"),
	];

	for (const p of candidates) {
		if (existsSync(p)) return p;
	}

	// Fall back to PATH
	try {
		const found = execFileSync("which", ["dp"], { stdio: "pipe" }).toString().trim();
		if (found) return found;
	} catch {}

	throw new Error(
		"dp CLI not installed. Install it first: https://devplatform.pages.devplatform.tcsbank.ru/spirit-user-docs/docs/cli/ — then run /login nestor again.",
	);
}

function dpEnv(): Record<string, string> {
	const env = { ...process.env as Record<string, string> };

	// Only set DP_WORKDIR if the nessy-managed directory exists.
	// System-installed dp (/usr/local/bin/dp) uses its own default workdir
	// and forcing a nonexistent path breaks auth state lookup.
	const nessyWorkdir = join(homedir(), ".nessy", DP_WORKDIR_NAME);
	if (existsSync(nessyWorkdir)) {
		env.DP_WORKDIR = nessyWorkdir;
	}

	return env;
}

// =============================================================================
// Token Management
// =============================================================================

function getDpToken(dpPath: string): string {
	let raw: string;
	let stderr = "";
	try {
		const result = execFileSync(dpPath, ["auth", "print-token"], {
			stdio: ["ignore", "pipe", "pipe"],
			timeout: 5_000,
			env: dpEnv(),
		});
		raw = result.toString().trim();
	} catch (e: any) {
		// dp exits non-zero when not logged in, with the error on stderr
		stderr = e?.stderr?.toString?.() ?? "";
		raw = e?.stdout?.toString?.()?.trim() ?? "";
	}

	// Not logged in: dp writes "no access token found..." to stderr
	// and may also output error text to stdout
	if (
		!raw ||
		raw.includes("no access token") ||
		raw.includes("authorize") ||
		stderr.includes("no access token") ||
		raw.length < 10  // valid tokens are always long
	) {
		throw new Error("not-logged-in");
	}
	return raw;
}

async function exchangeForJwt(dpToken: string): Promise<{ jwt: string; expiresAt: number }> {
	const res = await fetch(TOKEN_ENDPOINT, {
		method: "POST",
		headers: {
			"Content-Type": "application/json",
			Authorization: `Bearer ${dpToken}`,
			"X-Request-Id": crypto.randomUUID(),
		},
		body: "{}",
	});

	if (!res.ok) {
		const body = await res.text();
		throw new Error(
			`Token exchange failed (${res.status}): ${body || "(empty response)"}\n` +
			`Endpoint: ${TOKEN_ENDPOINT}\n` +
			`DP token prefix: ${dpToken.substring(0, 20)}...`,
		);
	}

	const data = (await res.json()) as {
		jwt: string;
		token: { expires_at: string };
	};

	return {
		jwt: data.jwt,
		expiresAt: new Date(data.token.expires_at).getTime(),
	};
}

// =============================================================================
// Model Discovery
// =============================================================================

interface NestorModel {
	name: string;
	desc?: string;
	is_default?: boolean;
}

async function fetchNestorModels(jwt: string): Promise<NestorModel[]> {
	const res = await fetch(MODELS_ENDPOINT, {
		headers: { "Content-Type": "application/json", "Nestor-Token": jwt },
	});

	if (!res.ok) return [];

	const data = await res.json();
	return Array.isArray(data) ? data : (data as { models?: NestorModel[] }).models ?? [];
}

// =============================================================================
// OAuth Integration (factory — creates session-scoped closures)
// =============================================================================

function createOAuthHandlers(piRef: ExtensionAPI) {
	let dpPath: string | undefined;

	async function updateModels(jwt: string): Promise<void> {
		const nestorModels = await fetchNestorModels(jwt);
		if (nestorModels.length === 0) return;

		const overrides = loadModelOverrides();

		piRef.registerProvider("nestor", {
			baseUrl: API_BASE,
			api: "nestor" as any,
			models: nestorModels.map((m) => {
				const id = m.name.toLowerCase();
				const override = findOverride(id, overrides);

				const isVision = inferVision(id, override);
				const isReasoning = inferReasoning(id, override);
				const contextWindow = inferContextWindow(id, override);
				const maxTokens = inferMaxTokens(id, override);
				const thinkingFormat = inferThinkingFormat(id, override);

				return {
					id: m.name,
					name: m.desc || m.name,
					reasoning: isReasoning,
					input: (isVision ? ["text", "image"] : ["text"]) as ("text" | "image")[],
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
					contextWindow,
					maxTokens,
					compat: {
						maxTokensField: "max_tokens" as const,
						supportsDeveloperRole: false,
						...(thinkingFormat && { thinkingFormat: thinkingFormat as any }),
					},
				};
			}),
			oauth: { name: "Nestor (DP Auth)", login, refreshToken, getApiKey },
			streamSimple: streamNestor,
		});
	}

	async function login(callbacks: OAuthLoginCallbacks): Promise<OAuthCredentials> {
		dpPath = findDpBinary();

		// Try existing DP session first
		try {
			const dpToken = getDpToken(dpPath);
			const { jwt, expiresAt } = await exchangeForJwt(dpToken);
			await updateModels(jwt);
			return { refresh: "dp-session", access: jwt, expires: expiresAt };
		} catch (e) {
			// Only fall through to interactive login if not logged in.
			// Propagate real errors (network, malformed response, etc.)
			if (!(e instanceof Error) || e.message !== "not-logged-in") {
				throw e;
			}
		}

		// Launch dp auth login automatically — it opens the browser via the OS
		// (no TTY needed for that part). Detach so it doesn't block pi's TUI.
		let launched = false;
		try {
			const { spawn: _spawn } = await import("node:child_process");
			const loginProc = _spawn(dpPath, ["auth", "login"], {
				stdio: "ignore",
				detached: true,
				env: dpEnv(),
			});
			loginProc.unref();
			launched = true;
		} catch {}

		await callbacks.onPrompt({
			message: launched
				? "Opening browser for DP authentication...\nComplete the login, then press Enter."
				: "No active DP session. Run `dp auth login` in another terminal, then press Enter.",
		});

		// Retry after user says they've logged in
		let dpToken: string;
		try {
			dpToken = getDpToken(dpPath);
		} catch {
			throw new Error(
				"Still no valid DP session. Make sure 'dp auth login' completed successfully.",
			);
		}

		const { jwt, expiresAt } = await exchangeForJwt(dpToken);
		await updateModels(jwt);

		return { refresh: "dp-session", access: jwt, expires: expiresAt };
	}

	async function refreshToken(credentials: OAuthCredentials): Promise<OAuthCredentials> {
		dpPath = dpPath || findDpBinary();

		// The DP session persists independently — just get a fresh token
		const dpToken = getDpToken(dpPath);
		const { jwt, expiresAt } = await exchangeForJwt(dpToken);

		return { refresh: "dp-session", access: jwt, expires: expiresAt };
	}

	function getApiKey(credentials: OAuthCredentials): string {
		return credentials.access;
	}

	async function autoLogin(): Promise<void> {
		dpPath = findDpBinary();
		const dpToken = getDpToken(dpPath);
		const { jwt } = await exchangeForJwt(dpToken);
		await updateModels(jwt);
	}

	return { login, refreshToken, getApiKey, updateModels, autoLogin };
}

// =============================================================================
// Model Capability Overrides (from .pi/settings.json)
// =============================================================================

// Users can override inferred capabilities in .pi/settings.json:
//
//   {
//     "nestor": {
//       "modelOverrides": {
//         "exact-model-name": {
//           "contextWindow": 131072,
//           "maxTokens": 8192,
//           "reasoning": true,
//           "vision": true,
//           "thinkingFormat": "qwen"
//         }
//       }
//     }
//   }
//
// Keys are matched against model IDs: first by exact match (case-insensitive),
// then by substring containment. First match wins.

interface ModelOverride {
	contextWindow?: number;
	maxTokens?: number;
	reasoning?: boolean;
	vision?: boolean;
	thinkingFormat?: string;
}

function loadModelOverrides(): Record<string, ModelOverride> {
	try {
		const settingsPath = join(process.cwd(), ".pi", "settings.json");
		if (!existsSync(settingsPath)) return {};
		const raw = JSON.parse(readFileSync(settingsPath, "utf-8"));
		const overrides = raw?.nestor?.modelOverrides;
		if (!overrides || typeof overrides !== "object") return {};
		return overrides as Record<string, ModelOverride>;
	} catch {
		return {};
	}
}

function findOverride(id: string, overrides: Record<string, ModelOverride>): ModelOverride | undefined {
	const lower = id.toLowerCase();
	// Exact match (case-insensitive)
	for (const [pattern, override] of Object.entries(overrides)) {
		if (pattern.toLowerCase() === lower) return override;
	}
	// Substring match
	for (const [pattern, override] of Object.entries(overrides)) {
		if (lower.includes(pattern.toLowerCase())) return override;
	}
	return undefined;
}

// =============================================================================
// Model Capability Inference
// =============================================================================

// The Nestor API doesn't return context window or max output tokens,
// so we infer from model name patterns. Config overrides take precedence.

function inferContextWindow(id: string, override?: ModelOverride): number {
	if (override?.contextWindow != null) return override.contextWindow;
	if (id.includes("qwen3") && id.includes("35")) return 1_048_576; // Qwen 3.5: 1M
	if (id.includes("qwen3")) return 131_072; // Qwen 3: 128k
	if (id.includes("qwen2.5")) return 131_072;
	if (id.includes("gpt-4o") || id.includes("gpt-4-o")) return 128_000;
	if (id.includes("gpt-oss")) return 128_000;
	if (id.includes("deepseek")) return 128_000;
	return 128_000; // safe default
}

function inferMaxTokens(id: string, override?: ModelOverride): number {
	if (override?.maxTokens != null) return override.maxTokens;
	if (id.includes("qwen3") && id.includes("35")) return 16_384;
	if (id.includes("qwen3")) return 8_192;
	if (id.includes("gpt")) return 4_096;
	return 8_192; // safe default
}

function inferReasoning(id: string, override?: ModelOverride): boolean {
	if (override?.reasoning != null) return override.reasoning;
	const isQwen = id.includes("qwen");
	return isQwen || id.includes("think") || id.includes("reason");
}

function inferVision(id: string, override?: ModelOverride): boolean {
	if (override?.vision != null) return override.vision;
	return id.includes("-vl") || id.includes("vision");
}

function inferThinkingFormat(id: string, override?: ModelOverride): string | undefined {
	if (override?.thinkingFormat != null) return override.thinkingFormat;
	if (id.includes("qwen")) return "qwen";
	return undefined;
}

// =============================================================================
// Think Tag Parser
// =============================================================================

// The Nestor API may return thinking in two ways:
// A) Structured: reasoning_content field → Pi handles natively
// B) Inline: <think>...</think> tags in content (sometimes missing <think>)
//
// This interceptor handles case B. It uses a state machine:
//   BUFFERING → accumulates text, waiting to detect which mode
//   THINKING  → inside a <think> block, streaming as thinking_delta
//   TEXT      → normal text, streaming as text_delta
//   DISABLED  → native thinking detected, pass everything through
//
// The BUFFERING phase handles the missing <think> tag case: all text
// is held until we see <think>, </think>, or get a signal that thinking
// is native. No text is emitted during BUFFERING, so we can retroactively
// create thinking blocks from the buffered content.

type Phase = "buffering" | "thinking" | "text" | "disabled";

function createThinkTagInterceptor(output: { content: any[] }) {
	let phase: Phase = "buffering";
	let buffer = "";
	let thinkIdx: number | null = null;
	let textIdx: number | null = null;

	function pushThinkStart(events: any[], partial: any) {
		const block = { type: "thinking", thinking: "" };
		output.content.push(block);
		thinkIdx = output.content.length - 1;
		events.push({ type: "thinking_start", contentIndex: thinkIdx, partial });
	}

	function pushThinkDelta(events: any[], text: string, partial: any) {
		if (!text || thinkIdx === null) return;
		const block = output.content[thinkIdx];
		if (block?.type === "thinking") block.thinking += text;
		events.push({ type: "thinking_delta", contentIndex: thinkIdx, delta: text, partial });
	}

	function pushThinkEnd(events: any[], partial: any) {
		if (thinkIdx === null) return;
		const block = output.content[thinkIdx];
		events.push({ type: "thinking_end", contentIndex: thinkIdx, content: block?.thinking ?? "", partial });
		thinkIdx = null;
	}

	function pushTextStart(events: any[], partial: any) {
		if (textIdx !== null) return;
		const block = { type: "text", text: "" };
		output.content.push(block);
		textIdx = output.content.length - 1;
		events.push({ type: "text_start", contentIndex: textIdx, partial });
	}

	function pushTextDelta(events: any[], text: string, partial: any) {
		if (!text || textIdx === null) return;
		const block = output.content[textIdx];
		if (block?.type === "text") block.text += text;
		events.push({ type: "text_delta", contentIndex: textIdx, delta: text, partial });
	}

	// Called when the inner stream emits thinking_start — API handles thinking natively
	function disableInterception(): any[] {
		// Flush buffer as text since it's the response, not thinking
		const events: any[] = [];
		if (buffer) {
			// This text arrived before we knew thinking was native.
			// It's response text — will be re-emitted by the caller.
		}
		phase = "disabled";
		return events;
	}

	function processTextDelta(delta: string, _contentIndex: number, partial: any): any[] {
		if (phase === "disabled") return [];

		const events: any[] = [];
		buffer += delta;

		// In BUFFERING phase, scan buffer for tags to decide mode
		if (phase === "buffering") {
			// Check for <think> tag
			const openIdx = buffer.indexOf("<think>");
			if (openIdx !== -1) {
				// Text before <think> (if any) is regular text
				if (openIdx > 0) {
					pushTextStart(events, partial);
					pushTextDelta(events, buffer.substring(0, openIdx), partial);
				}
				buffer = buffer.substring(openIdx + 7);
				phase = "thinking";
				pushThinkStart(events, partial);
				// Fall through to process remaining buffer in THINKING phase
			}
			// Check for </think> without <think> (missing open tag)
			else {
				const closeIdx = buffer.indexOf("</think>");
				if (closeIdx !== -1) {
					// Everything before </think> is thinking content
					pushThinkStart(events, partial);
					pushThinkDelta(events, buffer.substring(0, closeIdx), partial);
					pushThinkEnd(events, partial);
					buffer = buffer.substring(closeIdx + 8);
					phase = "text";
					// Fall through to process remaining buffer in TEXT phase
				} else {
					// Still waiting — keep buffering
					return events;
				}
			}
		}

		// Process remaining buffer in current phase
		while (buffer.length > 0) {
			if (phase === "thinking") {
				const closeIdx = buffer.indexOf("</think>");
				if (closeIdx === -1) {
					// Keep last 8 chars for partial </think> detection
					const safe = Math.max(0, buffer.length - 8);
					if (safe > 0) {
						pushThinkDelta(events, buffer.substring(0, safe), partial);
						buffer = buffer.substring(safe);
					}
					break;
				}
				pushThinkDelta(events, buffer.substring(0, closeIdx), partial);
				pushThinkEnd(events, partial);
				buffer = buffer.substring(closeIdx + 8);
				phase = "text";
			} else if (phase === "text") {
				const openIdx = buffer.indexOf("<think>");
				if (openIdx === -1) {
					const safe = Math.max(0, buffer.length - 6);
					if (safe > 0) {
						pushTextStart(events, partial);
						pushTextDelta(events, buffer.substring(0, safe), partial);
						buffer = buffer.substring(safe);
					}
					break;
				}
				if (openIdx > 0) {
					pushTextStart(events, partial);
					pushTextDelta(events, buffer.substring(0, openIdx), partial);
				}
				buffer = buffer.substring(openIdx + 7);
				phase = "thinking";
				pushThinkStart(events, partial);
			} else {
				break;
			}
		}

		return events;
	}

	function flush(_contentIndex: number, partial: any): any[] {
		if (phase === "disabled") return [];
		const events: any[] = [];
		if (buffer.length > 0) {
			if (phase === "thinking") {
				pushThinkDelta(events, buffer, partial);
				pushThinkEnd(events, partial);
			} else {
				// BUFFERING or TEXT — emit as text
				pushTextStart(events, partial);
				pushTextDelta(events, buffer, partial);
			}
			buffer = "";
		}
		return events;
	}

	// Get the buffered text (for flushing when switching to native mode)
	function getBuffer(): string { return buffer; }
	function clearBuffer(): void { buffer = ""; }

	return { processTextDelta, flush, disableInterception, getBuffer, clearBuffer, getPhase: () => phase };
}

// =============================================================================
// Stream Function
// =============================================================================

function streamNestor(
	model: Model<Api>,
	context: Context,
	options?: SimpleStreamOptions,
): AssistantMessageEventStream {
	const stream = createAssistantMessageEventStream();

	(async () => {
		try {
			const jwt = options?.apiKey;
			if (!jwt) {
				throw new Error("Not authenticated. Run /login nestor");
			}

			const strippedId = model.id.replace(/^nestor\//, "");
			const modelWithBaseUrl = { ...model, id: strippedId, baseUrl: API_BASE };
			// Track whether thinking was requested so we can start the
			// interceptor in thinking mode for Qwen (handles missing <think>).
			// Uses a ref because onPayload fires during the first stream
			// iteration, after the interceptor is created.
			const thinkingRef = { enabled: false };
			const innerStream = streamSimpleOpenAICompletions(
				modelWithBaseUrl as Model<"openai-completions">,
				context,
				{
					...options,
					apiKey: "nestor-dp-auth",
					headers: {
						...options?.headers,
						"Nestor-Token": jwt,
					},
					onPayload: (payload: unknown) => {
						const p = payload as Record<string, unknown>;
						if (p.enable_thinking === true) thinkingRef.enabled = true;
						// Force one tool call per response. Part of the OpenAI chat
						// completions spec (not a sampling param), so the API should
						// respect it even if it ignores temperature/top_k/etc.
						if (p.tools && (p.tools as unknown[]).length > 0) {
							p.parallel_tool_calls = false;
						}
						return p;
					},
				},
			);

			// The Nestor API may return thinking two ways:
			// A) Structured: reasoning_content → Pi emits thinking events natively
			// B) Inline: <think>...</think> tags in content (sometimes missing <think>)
			//
			// The interceptor buffers text until it can determine the mode.
			// If thinking_start arrives from the inner stream, the interceptor
			// is disabled and text passes through normally.
			const cleanOutput = { content: [] as any[] };
			const interceptor = createThinkTagInterceptor(cleanOutput);
			// NOTE: can't cache thinkingRef.enabled here — onPayload hasn't
			// fired yet. It fires during the first stream iteration. Check
			// thinkingRef.enabled inside the loop instead.
			let intercepting = false;
			let interceptDecided = false;

			for await (const event of innerStream) {
				// Lazy decision: on the first event, onPayload has already
				// fired, so thinkingRef.enabled is correct.
				if (!interceptDecided) {
					interceptDecided = true;
					intercepting = thinkingRef.enabled;
				}

				// If the inner stream emits native thinking events, disable
				// our interception and flush any buffered text as-is.
				if (intercepting && event.type === "thinking_start") {
					const buffered = interceptor.getBuffer();
					interceptor.disableInterception();
					intercepting = false;
					// The buffered text is response content that arrived before
					// thinking_start. It will come again as text_delta from the
					// inner stream (we suppressed text_start but the deltas
					// were consumed by the interceptor). Re-emit it now.
					// Actually, the text_deltas were intercepted (continue'd),
					// so we need to re-emit the buffer as text events.
					if (buffered) {
						const partial = "partial" in event ? event.partial : undefined;
						stream.push({ type: "text_start", contentIndex: 0, partial } as any);
						stream.push({ type: "text_delta", contentIndex: 0, delta: buffered, partial } as any);
					}
				}

				if (!intercepting) {
					// Pass through everything when not intercepting
					stream.push(event);
					continue;
				}

				// Suppress inner stream's text_start/text_end — the interceptor
				// emits its own content blocks from a clean array.
				if (event.type === "text_start" || event.type === "text_end") {
					continue;
				}

				// Intercept text deltas
				if (event.type === "text_delta" && "delta" in event && typeof event.delta === "string") {
					const partial = "partial" in event ? event.partial : undefined;
					const events = interceptor.processTextDelta(
						event.delta,
						"contentIndex" in event ? (event as any).contentIndex : 0,
						partial,
					);
					if (partial?.content) partial.content = cleanOutput.content;
					for (const e of events) {
						(e as any).partial = partial;
						stream.push(e as any);
					}
					continue;
				}

				// On stream end, flush remaining interceptor buffer
				if (event.type === "done" || event.type === "error") {
					const partial = "partial" in event ? (event as any).partial : undefined;
					const flushed = interceptor.flush(0, partial);
					if (partial?.content) partial.content = cleanOutput.content;
					for (const e of flushed) {
						(e as any).partial = partial;
						stream.push(e as any);
					}
					if (event.type === "done" && (event as any).message?.content) {
						(event as any).message.content = cleanOutput.content;
					}
				}

				// Pass through non-text events (start, done, error, tool calls)
				stream.push(event);
			}
			stream.end();
		} catch (error) {
			stream.push({
				type: "error",
				reason: "error",
				error: {
					role: "assistant",
					content: [],
					api: model.api,
					provider: model.provider,
					model: model.id,
					usage: {
						input: 0,
						output: 0,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					},
					stopReason: "error",
					errorMessage: error instanceof Error ? error.message : String(error),
					timestamp: Date.now(),
				},
			});
			stream.end();
		}
	})();

	return stream;
}

// =============================================================================
// Extension Entry Point
// =============================================================================

function readDefaultModel(): string {
	// Read .pi/settings.json to find the configured default model
	// so we can register a placeholder with the correct ID
	try {
		const settingsPath = join(process.cwd(), ".pi", "settings.json");
		if (existsSync(settingsPath)) {
			const raw = JSON.parse(readFileSync(settingsPath, "utf-8"));
			if (raw.defaultModel) return raw.defaultModel;
		}
	} catch {}
	return "default";
}

export default function (pi: ExtensionAPI) {
	const { login, refreshToken, getApiKey, autoLogin } = createOAuthHandlers(pi);

	const defaultModelId = readDefaultModel();
	const overrides = loadModelOverrides();
	const defaultOverride = findOverride(defaultModelId.toLowerCase(), overrides);
	const defaultThinkingFormat = inferThinkingFormat(defaultModelId.toLowerCase(), defaultOverride);

	// Register placeholder with the actual model ID from settings
	// so Pi can resolve defaultModel immediately at startup.
	// Real model list replaces this after auto-login in session_start.
	pi.registerProvider("nestor", {
		baseUrl: API_BASE,
		apiKey: "NESTOR_JWT",
		api: "nestor" as any,
		models: [
			{
				id: defaultModelId,
				name: `${defaultModelId} (authenticating...)`,
				reasoning: inferReasoning(defaultModelId.toLowerCase(), defaultOverride),
				input: (inferVision(defaultModelId.toLowerCase(), defaultOverride) ? ["text", "image"] : ["text"]) as ("text" | "image")[],
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
				contextWindow: inferContextWindow(defaultModelId.toLowerCase(), defaultOverride),
				maxTokens: inferMaxTokens(defaultModelId.toLowerCase(), defaultOverride),
				compat: {
					maxTokensField: "max_tokens",
					supportsDeveloperRole: false,
					...(defaultThinkingFormat && { thinkingFormat: defaultThinkingFormat as any }),
				},
			},
		],
		oauth: {
			name: "Nestor (DP Auth)",
			login,
			refreshToken,
			getApiKey,
		},
		streamSimple: streamNestor,
	});

	// Auto-login on session start: silently try existing DP session
	// so real models are available without explicit /login
	pi.on("session_start", async (_event, _ctx) => {
		try {
			await autoLogin();
		} catch {
			// No active DP session — user will need /login nestor
		}
	});
}
