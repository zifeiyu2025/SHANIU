// 真机验收：只读状态探针（不发任务）。
// 用途：全新数据根 / 旧配置迁移两次启动只验「默认档与生效机制」，不消耗模型轮次。
import fs from "node:fs";

const CDP_PORT = Number(process.env.CDP_PORT || 9223);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function pickTarget() {
  for (let i = 0; i < 120; i += 1) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      const page = targets.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
      if (page) return page;
    } catch {}
    await sleep(1000);
  }
  throw new Error("CDP 目标未就绪");
}

function connect(url) {
  const ws = new WebSocket(url);
  let seq = 0;
  const pending = new Map();
  ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) reject(new Error(JSON.stringify(msg.error)));
      else resolve(msg.result);
    }
  };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
  return {
    ready,
    send(method, params) {
      seq += 1;
      return new Promise((resolve, reject) => {
        pending.set(seq, { resolve, reject });
        ws.send(JSON.stringify({ id: seq, method, params }));
      });
    },
    close: () => ws.close(),
  };
}

async function main() {
  const target = await pickTarget();
  console.error(`[status] target: ${target.url}`);
  const client = connect(target.webSocketDebuggerUrl);
  await client.ready;

  const evaluate = async (expression) => {
    const result = await client.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (result.exceptionDetails) {
      throw new Error(`JS 异常: ${JSON.stringify(result.exceptionDetails)}`);
    }
    return result.result.value;
  };

  for (let i = 0; i < 120; i += 1) {
    const ok = await evaluate("!!window.__TAURI_INTERNALS__").catch(() => false);
    if (ok) break;
    await sleep(500);
  }

  const status = await evaluate(
    "window.__TAURI_INTERNALS__.invoke('sandbox_status')",
  );
  console.log(`[status] sandbox_status: ${JSON.stringify(status)}`);
  client.close();
}

main().catch((error) => {
  console.error(`[status] FAILED: ${error.stack || error.message}`);
  process.exit(1);
});
