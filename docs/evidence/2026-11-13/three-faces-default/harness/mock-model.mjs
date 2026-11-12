// 第三批真机验收用的 mock 模型端点（OpenAI chat-completions 兼容，流式）。
// 只按轮次把预设工具调用发回，不触达外网。
// 第 1 步是 `terminal open`，之后的 `write/read/close` 需要上一轮返回的 session id，
// 所以这里从已有的 tool 消息里把 `term-<ULID>` 抠出来再用——mock 也要能读结果。
import http from "node:http";
import os from "node:os";
import path from "node:path";

const PORT = Number(process.env.MOCK_PORT || 18765);
// 不依赖调用方 shell 的环境变量（Win32_Process.Create 不继承它），给等价默认值。
const WORKSPACE =
  process.env.WORKSPACE ||
  "D:\\MyWorkspace\\SHANIU\\docs\\evidence\\2026-11-13\\three-faces-default\\workspace";
const OUTSIDE = process.env.OUTSIDE || os.homedir();
const OUTSIDE_FILE = path.join(OUTSIDE, "shaniu-mcp-escaped.txt");

let round = 0;
const log = (...args) => console.log(new Date().toISOString(), ...args);

function terminalIdFrom(messages) {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i];
    if (message.role !== "tool") continue;
    const text =
      typeof message.content === "string" ? message.content : JSON.stringify(message.content);
    const hit = /term-[0-9A-HJKMNP-TV-Z]{26}/.exec(text);
    if (hit) return hit[0];
  }
  return null;
}

const STEPS = [
  // ── 终端面（默认档 = 写约束受限令牌）────────────────────────────
  () => ({ name: "terminal", args: { action: "open", cwd: "." } }),
  (ctx) => ({
    name: "terminal",
    args: { action: "write", session_id: ctx.termId, data: "echo TERM-REAL-OK\r\n" },
  }),
  (ctx) => ({
    name: "terminal",
    args: { action: "write", session_id: ctx.termId, data: "echo term> term-out.txt\r\n" },
  }),
  (ctx) => ({ name: "terminal", args: { action: "read", session_id: ctx.termId, timeout_ms: 4000 } }),
  (ctx) => ({
    name: "terminal",
    args: { action: "write", session_id: ctx.termId, data: "exit\r\n" },
  }),
  (ctx) => ({ name: "terminal", args: { action: "read", session_id: ctx.termId, timeout_ms: 4000 } }),
  (ctx) => ({ name: "terminal", args: { action: "close", session_id: ctx.termId } }),
  // ── MCP server 面（同一默认档）──────────────────────────────────
  () => ({ name: "mcp__fs__read_text_file", args: { path: `${WORKSPACE}\\hello.txt` } }),
  () => ({
    name: "mcp__fs__write_file",
    args: { path: `${WORKSPACE}\\mcp-inside.txt`, content: "mcp-inside-ok" },
  }),
  () => ({
    name: "mcp__fs__write_file",
    args: { path: OUTSIDE_FILE, content: "should-be-denied" },
  }),
  () => ({ name: "mcp__fs__list_directory", args: { path: WORKSPACE } }),
];

function sse(res, payload) {
  res.write(`data: ${JSON.stringify(payload)}\n\n`);
}

function toolCallPayload(step) {
  return {
    id: "mock-1",
    choices: [
      {
        index: 0,
        delta: {
          tool_calls: [
            {
              index: 0,
              id: `call_${round}`,
              type: "function",
              function: { name: step.name, arguments: JSON.stringify(step.args) },
            },
          ],
        },
        finish_reason: null,
      },
    ],
  };
}

const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (chunk) => (body += chunk));
  req.on("end", () => {
    if (req.url.includes("/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ data: [{ id: "mock-acceptance", object: "model" }] }));
      return;
    }
    if (!req.url.includes("/chat/completions")) {
      res.writeHead(404).end("not found");
      return;
    }
    let payload = {};
    try {
      payload = JSON.parse(body || "{}");
    } catch {}
    const messages = payload.messages || [];
    const toolResults = messages.filter((m) => m.role === "tool").length;
    const stream = payload.stream !== false;
    const ctx = { termId: terminalIdFrom(messages) };
    const step = toolResults < STEPS.length ? STEPS[toolResults](ctx) : null;
    log("request", { toolResults, stream, termId: ctx.termId, step: step && step.name });

    if (!stream) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify(
          step
            ? {
                id: "mock-1",
                choices: [
                  {
                    index: 0,
                    message: {
                      role: "assistant",
                      content: null,
                      tool_calls: [
                        {
                          id: `call_${round}`,
                          type: "function",
                          function: { name: step.name, arguments: JSON.stringify(step.args) },
                        },
                      ],
                    },
                    finish_reason: "tool_calls",
                  },
                ],
              }
            : {
                id: "mock-1",
                choices: [
                  {
                    index: 0,
                    message: { role: "assistant", content: "ACCEPTANCE-DONE" },
                    finish_reason: "stop",
                  },
                ],
              },
        ),
      );
      return;
    }

    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    if (step) {
      round += 1;
      log("tool_call", round, step.name, JSON.stringify(step.args));
      sse(res, toolCallPayload(step));
      sse(res, { choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
    } else {
      log("final text");
      sse(res, { choices: [{ index: 0, delta: { content: "ACCEPTANCE-DONE" }, finish_reason: null }] });
      sse(res, { choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
    }
    res.write("data: [DONE]\n\n");
    res.end();
  });
});

server.listen(PORT, "127.0.0.1", () => log(`mock model listening on ${PORT}`));
