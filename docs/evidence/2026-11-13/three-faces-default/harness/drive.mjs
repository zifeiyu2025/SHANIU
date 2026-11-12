// 第三批真机验收驱动器：连真实 exe 的 CDP，走前端同一条 IPC（__TAURI_INTERNALS__.invoke）。
// 1) 存假 Key、读 sandbox_status；2) MCP 首连审查 + 批准（真实指纹）；3) 发任务等收口；4) 打印 rollout。
import fs from "node:fs";

const CDP_PORT = Number(process.env.CDP_PORT || 9224);
const DATA_ROOT = process.env.DATA_ROOT || "";
const SERVER_ID = process.env.MCP_SERVER_ID || "fs";

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

  // ── MCP 首连审查 + 批准（真实指纹，真实进程）────────────────────
  // acceptNewBinary=true：配置/参数变过时需要用户显式接受新二进制再审查。
  const review = await invoke("mcp_connect_and_review", {
    serverId: SERVER_ID,
    acceptNewBinary: true,
  });
  console.log(
    `[drive] mcp review: status=${review.status} exec=${review.executable} tools=${review.tools
      .map((tool) => tool.dispatch_name)
      .join(",")}`,
  );
  console.log(`[drive] mcp sandbox: ${JSON.stringify(review.sandbox)}`);
  console.log(`[drive] mcp line: ${review.sandbox && review.sandbox.kind}`);
  const decisions = review.tools
    .filter((tool) => tool.guard_hits.length === 0)
    .map((tool) => ({
      dispatch_name: tool.dispatch_name,
      approved: true,
      risk: tool.dispatch_name.includes("write") ? "write_in_workspace" : "read",
    }));
  if (review.status === "review") {
    const resolved = await invoke("mcp_resolve_trust", {
      serverId: SERVER_ID,
      fingerprint: review.fingerprint,
      decisions,
    });
    console.log(
      `[drive] mcp resolve: status=${resolved.status} approved=${resolved.approved_tools.length}`,
    );
  } else {
    console.log(`[drive] mcp resolve skipped: 状态已是 ${review.status}（无待审会话）`);
  }

  const servers = await invoke("mcp_list_servers");
  console.log(`[drive] mcp_list_servers: ${JSON.stringify(servers)}`);

  // ── 一条任务跑完三面 ───────────────────────────────────────────
  const sessionId = await invoke("send_task", {
    task: "ACCEPTANCE-RUN（按预设工具逐条执行，不要解释）",
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
