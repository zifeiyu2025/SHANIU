// 真机验收驱动器：连真实 exe 的 CDP，走前端同一条 IPC（__TAURI_INTERNALS__.invoke）。
// 只做四件事：存一个假 Key、读 sandbox_status、发一条任务、等任务收口后打印轨迹。
import fs from "node:fs";

const CDP_PORT = Number(process.env.CDP_PORT || 9223);
const DATA_ROOT = process.env.DATA_ROOT || "";

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
  console.error(`[drive] target: ${target.url}`);
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

  const invoke = (cmd, args = {}) =>
    evaluate(
      `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})`,
    );

  // 等前端就绪
  for (let i = 0; i < 120; i += 1) {
    const ok = await evaluate("!!window.__TAURI_INTERNALS__").catch(() => false);
    if (ok) break;
    await sleep(500);
  }

  try {
    await invoke("store_api_key", { provider: "openai-chat", key: "sk-acceptance-mock" });
    console.log("[drive] store_api_key: ok");
  } catch (error) {
    console.log(`[drive] store_api_key: ${error.message}`);
  }

  const status = await invoke("sandbox_status");
  console.log(`[drive] sandbox_status: ${JSON.stringify(status)}`);

  const sessionId = await invoke("send_task", {
    task: "ACCEPTANCE-RUN（按预设命令逐条执行，不要解释）",
    agentId: null,
    permissionPreset: null,
    images: null,
  });
  console.log(`[drive] session: ${sessionId}`);

  let wasBusy = false;
  for (let i = 0; i < 400; i += 1) {
    await sleep(2000);
    const survey = await invoke("quit_survey");
    if (survey.busy) wasBusy = true;
    if (wasBusy && !survey.busy) break;
    if (!wasBusy && i > 10) break;
    if (i % 5 === 0) console.log(`[drive] waiting #${i} busy=${survey.busy}`);
  }

  console.log(`[drive] session finished: ${sessionId}`);
  if (DATA_ROOT) {
    const path = `${DATA_ROOT}/rollouts/${sessionId}.jsonl`;
    if (fs.existsSync(path)) {
      console.log(`[drive] rollout: ${path}`);
      const lines = fs.readFileSync(path, "utf8").split(/\r?\n/).filter(Boolean);
      for (const line of lines) {
        console.log(`[rollout] ${line}`);
      }
    } else {
      console.log(`[drive] rollout 缺失: ${path}`);
    }
  }
  client.close();
}

main().catch((error) => {
  console.error(`[drive] FAILED: ${error.stack || error.message}`);
  process.exit(1);
});
