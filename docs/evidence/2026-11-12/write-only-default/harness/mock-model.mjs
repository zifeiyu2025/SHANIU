// 真机验收用的 mock 模型端点（OpenAI chat-completions 兼容，流式）。
// 不做任何外网请求；只按轮次把预设命令作为 unified_exec 工具调用发回。
import http from "node:http";

const PORT = Number(process.env.MOCK_PORT || 18765);

// 每一轮 unified_exec 的命令（cmd 语法）。顺序即执行顺序。
const COMMANDS = [
  // 1) 区内写
  String.raw`echo inside-write-ok> inside.txt && type inside.txt`,
  // 2) 区外写（预期被拒）
  String.raw`echo escaped> "%USERPROFILE%\shaniu-accept-outside.txt"`,
  // 3) 读区外（预期成功）
  String.raw`type "%USERPROFILE%\shaniu-accept-read-marker.txt"`,
  // 4) node 孙进程：区内写
  String.raw`node -e "require('fs').writeFileSync('node-inside.txt','node-inside-ok')" && type node-inside.txt`,
  // 5) node 孙进程：区外写（预期被拒）
  String.raw`node -e "require('fs').writeFileSync(process.env.USERPROFILE+'\\shaniu-accept-node-outside.txt','x')"`,
  // 6) 区内 cargo build
  String.raw`cargo build`,
  // 7) 出网（node/BoringSSL）
  String.raw`node -e "require('https').get('https://example.com',r=>{console.log('STATUS='+r.statusCode);process.exit(0)}).on('error',e=>{console.log('ERR='+e.message);process.exit(1)})"`,
  // 8) 区内 cargo test（docs/16 §7 步骤 2 (c)：harness 能力基线）
  String.raw`cargo test`,
  // 9) 读系统目录（docs/16 §7 步骤 2 (f)：读隔离已拆，成功是预期）
  String.raw`type C:\Windows\win.ini`,
  // 10) node 孙进程 execSync（docs/16 §7 步骤 2 (d)：默认 DACL 生效时孙进程能建管道）
  String.raw`node -e "const{execSync}=require('child_process');console.log('SYNC='+execSync('echo ok').toString().trim())"`,
  // 11) 区内删除（docs/16 §7 步骤 2 (a) 的删除半边）
  String.raw`del node-inside.txt && echo DEL-OK`,
];

let round = 0;
const log = (...args) => console.log(new Date().toISOString(), ...args);

function sse(res, payload) {
  res.write(`data: ${JSON.stringify(payload)}\n\n`);
}

function toolCallResponse(res, command) {
  const args = JSON.stringify({ command });
  sse(res, {
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
              function: { name: "unified_exec", arguments: args },
            },
          ],
        },
        finish_reason: null,
      },
    ],
  });
  sse(res, { choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
  res.write("data: [DONE]\n\n");
  res.end();
}

function textResponse(res, text) {
  sse(res, { choices: [{ index: 0, delta: { content: text }, finish_reason: null }] });
  sse(res, { choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
  res.write("data: [DONE]\n\n");
  res.end();
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
    log("request", { toolResults, stream, msgs: messages.length });

    const command = COMMANDS[toolResults];
    if (!stream) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify(
          command
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
                          function: { name: "unified_exec", arguments: JSON.stringify({ command }) },
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
                  { index: 0, message: { role: "assistant", content: "ACCEPTANCE-DONE" }, finish_reason: "stop" },
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
    if (command) {
      round += 1;
      log("tool_call", round, command);
      toolCallResponse(res, command);
    } else {
      log("final text");
      textResponse(res, "ACCEPTANCE-DONE");
    }
  });
});

server.listen(PORT, "127.0.0.1", () => log(`mock model listening on ${PORT}`));
