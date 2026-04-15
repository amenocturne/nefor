/**
 * Mock LLM HTTP server implementing OpenAI Chat Completions API (SSE streaming).
 * No external dependencies — uses node:http only.
 */
import { createServer, type Server, type IncomingMessage, type ServerResponse } from "node:http";

export type ResponseHandler = (messages: Array<{ role: string; content: string }>) =>
  | { type: "text"; content: string }
  | { type: "tool_call"; name: string; arguments: Record<string, unknown>; id?: string };

export type RequestLogEntry = { messages: any[]; model: string };

export function createMockLLMServer(handler: ResponseHandler) {
  let server: Server;
  const requestLog: RequestLogEntry[] = [];

  function writeSSEChunk(res: ServerResponse, data: string) {
    res.write(`data: ${data}\n\n`);
  }

  function handleCompletions(req: IncomingMessage, res: ServerResponse) {
    let body = "";
    req.on("data", (chunk: Buffer) => { body += chunk.toString(); });
    req.on("end", () => {
      let parsed: any;
      try {
        parsed = JSON.parse(body);
      } catch {
        res.writeHead(400, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "Invalid JSON" }));
        return;
      }

      const { messages, model } = parsed;
      requestLog.push({ messages, model });

      const response = handler(messages);
      const chatId = `chatcmpl-${Date.now()}`;

      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        "Connection": "keep-alive",
      });

      if (response.type === "text") {
        // Stream text in small chunks to simulate realistic streaming
        const content = response.content;
        const chunkSize = 10;
        for (let i = 0; i < content.length; i += chunkSize) {
          const slice = content.slice(i, i + chunkSize);
          writeSSEChunk(res, JSON.stringify({
            id: chatId,
            object: "chat.completion.chunk",
            choices: [{ index: 0, delta: { content: slice }, finish_reason: null }],
          }));
        }

        // Finish chunk
        writeSSEChunk(res, JSON.stringify({
          id: chatId,
          object: "chat.completion.chunk",
          choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
        }));
      } else {
        // Tool call response
        const callId = response.id ?? `call_${Date.now()}`;
        const args = JSON.stringify(response.arguments);

        // First chunk: tool call start with function name
        writeSSEChunk(res, JSON.stringify({
          id: chatId,
          object: "chat.completion.chunk",
          choices: [{
            index: 0,
            delta: {
              tool_calls: [{
                index: 0,
                id: callId,
                type: "function",
                function: { name: response.name, arguments: "" },
              }],
            },
            finish_reason: null,
          }],
        }));

        // Second chunk: arguments
        writeSSEChunk(res, JSON.stringify({
          id: chatId,
          object: "chat.completion.chunk",
          choices: [{
            index: 0,
            delta: {
              tool_calls: [{
                index: 0,
                function: { arguments: args },
              }],
            },
            finish_reason: null,
          }],
        }));

        // Finish chunk
        writeSSEChunk(res, JSON.stringify({
          id: chatId,
          object: "chat.completion.chunk",
          choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }],
        }));
      }

      writeSSEChunk(res, "[DONE]");
      res.end();
    });
  }

  function handleRequest(req: IncomingMessage, res: ServerResponse) {
    // Handle models endpoint (Pi may query it)
    if (req.method === "GET" && req.url === "/v1/models") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({
        object: "list",
        data: [{ id: "mock-model", object: "model", owned_by: "mock-llm" }],
      }));
      return;
    }

    if (req.method === "POST" && req.url === "/v1/chat/completions") {
      handleCompletions(req, res);
      return;
    }

    res.writeHead(404, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "Not found" }));
  }

  return {
    requestLog,

    start(): Promise<{ port: number; url: string }> {
      return new Promise((resolve, reject) => {
        server = createServer(handleRequest);
        server.listen(0, "127.0.0.1", () => {
          const addr = server.address();
          if (!addr || typeof addr === "string") {
            reject(new Error("Failed to get server address"));
            return;
          }
          resolve({ port: addr.port, url: `http://127.0.0.1:${addr.port}` });
        });
        server.on("error", reject);
      });
    },

    stop(): Promise<void> {
      return new Promise((resolve, reject) => {
        if (!server) { resolve(); return; }
        server.close((err) => {
          if (err) reject(err);
          else resolve();
        });
      });
    },
  };
}
