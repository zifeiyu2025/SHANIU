#!/usr/bin/env node
// 任务级能力对照驱动器：同一批题、同一个模型、同一判据，分别跑 shaniu / codex / dsh。
//
// 用法：
//   node evals/compare/run-arm.mjs --arm codex --tasks repo-bug-compaction-tail --repeats 1
//   node evals/compare/run-arm.mjs --arm dsh --tasks a,b
//   node evals/compare/run-arm.mjs --arm shaniu --tasks a,b   # 转调 Rust 评测运行器
//   node evals/compare/run-arm.mjs --arm codex --tasks a --prepare-only   # 只准备+前置判据
//
// 口径（见同目录 README.md）：
//   - 工作区准备与 Rust 运行器逐步对齐（快照 → git init → 补丁 → fixture → AGENTS.md → 提交），
//     准备完打印 `git ls-files -s` 的 sha256，用来和 Rust 侧 `[prep] ... tree=` 对齐；
//   - 前置判据必须红，否则该题 invalid；
//   - 判据只认「退出码 0 且 stdout 含 REPO-CHECK: PASS」；
//   - 判据/守卫类失败（改测试区、删断言）单独记 tampered，且该题 invalid，不算能力失败；
//   - 每题保留 diff 摘要与原始事件流，token 取不到就写 null，不估算。

import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '../..');
const TASKS_FILE = path.join(ROOT, 'evals', 'repo-tasks.json');
const JUDGE_SCRIPT = path.join(ROOT, 'evals', 'repo', 'judge', 'run.ps1');
/** zcode 本地源码仓库（无 npm 发布包，必须源码构建后才能当对照臂）。 */
const ZCODE_REPO = process.env.ZCODE_REPO ?? 'D:\\AI\\ZCode';
const PASS_MARKER = 'REPO-CHECK: PASS';
// 守卫失败的语气：出现这些就说明测试区/判据被动过，按 invalid 处理而不是判能力失败。
const TAMPER_MARKERS = ['守卫目标缺失', '守卫目标无测试区', '测试区被改', '判据文件被改', '断言被删减'];
// 判据「自己跑挂了」的痕迹：磁盘满、目录被外部清掉、副本复制失败。
// 这类红不能算能力失败，也不能当可用结果——必须重判或重跑。
const ENV_FAIL_MARKERS = ['os error 112', 'os error 3', '磁盘空间不足', '系统找不到指定', 'robocopy'];
// 与 Rust 侧 EVAL_AGENTS_NOTE 逐字一致：三方看到的环境信息必须同一份。
// 注意：Rust 的字符串字面量用行尾 `\` 续行，会**吃掉下一行的前导空白**，
// 所以这里续行也不能有缩进——否则 AGENTS.md 的 blob 哈希会与 Rust 准备的工作区不同，
// 工作区指纹闸门（tree_fingerprint）会直接判不一致。
const AGENTS_NOTE = `
## 评测环境（评测器注入，勿改动本节）

- 本工作区的命令运行在 AppContainer 沙箱内。全量 \`cargo test\` 会有 41 条用例必然红，
全部落在 \`sandbox::*\`、\`tools::terminal\`、\`tools::unified_exec\`：它们要在沙箱里再创建
受限令牌、写 DACL，嵌套调用被系统拒绝（\`0x80070005\`、\`CreateRestrictedToken\` 参数错误
\`0x80070057\`、DACL 写拒绝）。这些用例在宿主机与 CI 上通过，**不是你的改动造成的回归**。
不要为它们修改 \`src-tauri/src/sandbox/**\`、\`tools/terminal*\`、\`tools/unified_exec*\` 等
与题目无关的文件。
- 收工验证用定向命令：\`cargo test --lib <模块前缀>\`、\`cargo test --test <集成测试名>\`、
\`cargo check --all-targets\`。除非题目明确要求，不要用全量 \`cargo test\`。
`;
const AGENTS_MARKER = '## 评测环境（评测器注入，勿改动本节）';

function parseArgs(argv) {
  const out = {
    arm: '',
    tasks: [],
    repeats: 1,
    model: 'deepseek-flash',
    out: '',
    prepareOnly: false,
    protocol: '',
    expectTree: new Map(),
    resume: false,
    resumeWs: '',
  };
  for (let i = 2; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      i += 1;
      return argv[i];
    };
    if (arg === '--arm') out.arm = next();
    else if (arg === '--tasks') out.tasks = next().split(',').map((s) => s.trim()).filter(Boolean);
    else if (arg === '--repeats') out.repeats = Math.max(1, Number.parseInt(next(), 10) || 1);
    else if (arg === '--model') out.model = next();
    else if (arg === '--out') out.out = next();
    else if (arg === '--prepare-only') out.prepareOnly = true;
    else if (arg === '--protocol') out.protocol = next();
    // 续跑：复用 outDir 下已有 ndjson/judge.log 的题，只跑缺口。重跑会覆盖已有 ndjson，
    // 所以「跳过已出结果的题」不是优化，是正确性要求。
    else if (arg === '--resume') out.resume = true;
    // 上一轮的工作区根目录（`<tmpRoot>/ws`）：用来重判「环境故障造成的假红」并回收空间。
    else if (arg === '--resume-ws') out.resumeWs = next();
    else if (arg === '--expect-tree') {
      // 形如 `task-id=sha256,task-id2=sha256`：把「同一批题在三方启动前是同一棵树」
      // 落成硬闸门，不一致直接判 invalid，而不是事后靠人眼比对两份 summary.json。
      for (const pair of next().split(',').map((s) => s.trim()).filter(Boolean)) {
        const at = pair.lastIndexOf('=');
        if (at <= 0) throw new Error(`--expect-tree 需要 task=hash：${pair}`);
        out.expectTree.set(pair.slice(0, at), pair.slice(at + 1));
      }
    } else throw new Error(`未知参数 ${arg}`);
  }
  if (!out.arm) throw new Error('缺少 --arm shaniu|codex|dsh|zcode');
  if (!['shaniu', 'codex', 'dsh', 'zcode'].includes(out.arm)) throw new Error(`未知臂 ${out.arm}`);
  return out;
}

function sh(cmd, args, opts = {}) {
  const started = Date.now();
  const res = spawnSync(cmd, args, {
    cwd: opts.cwd,
    env: opts.env ?? process.env,
    encoding: 'utf8',
    windowsHide: true,
    timeout: opts.timeoutMs ?? 120000,
    maxBuffer: 64 * 1024 * 1024,
  });
  return {
    code: res.status ?? -1,
    out: res.stdout ?? '',
    err: res.stderr ?? '',
    ms: Date.now() - started,
  };
}

function git(cwd, args, env) {
  // 固定身份与禁用分页：否则提交会因缺 user.name 而失败，log 会卡在 pager。
  return sh('git', ['-c', 'user.name=eval', '-c', 'user.email=eval@local', ...args], { cwd, env });
}

function sha256(text) {
  return createHash('sha256').update(text, 'utf8').digest('hex');
}

/** 临时目录所在盘的可用空间（GB）。上一轮就是在这里撑爆的，收尾行要带上前后对比。 */
function diskFreeGb() {
  try {
    const stat = fs.statfsSync(os.tmpdir());
    return ((stat.bavail * stat.bsize) / 1024 ** 3).toFixed(1);
  } catch {
    return 'N/A';
  }
}

function tail(text, limit = 400) {
  const clean = String(text).replace(/\r\n/g, '\n').trim();
  return clean.length <= limit ? clean : `…${clean.slice(-limit)}`;
}

function npmGlobalRoot() {
  // npm 在 Windows 上是 .cmd，不能直接 spawn；先按默认位置取，取不到再走 cmd /c。
  const appData = process.env.APPDATA;
  if (appData) {
    const guess = path.join(appData, 'npm', 'node_modules');
    if (fs.existsSync(guess)) return guess;
  }
  const res = sh('cmd', ['/c', 'npm', 'root', '-g']);
  return res.code === 0 ? res.out.trim() : '';
}

/** 解析 CLI 的真实入口，避开 Windows 下 .cmd shim 无法直接 spawn 的问题。 */
function resolveArm(arm) {
  if (arm === 'shaniu') return { command: 'cargo', entry: [] };
  // zcode 没有官方 npm 发布包（`@zcode/cli` 的 package.json 是 private），
  // 只能从源码构建；入口由 ZCODE_CLI_ENTRY 指定，默认 D:\AI\ZCode 的产物。
  if (arm === 'zcode') {
    const entry = process.env.ZCODE_CLI_ENTRY ?? path.join(ZCODE_REPO, 'apps', 'zcode-cli', 'packages', 'cli', 'dist', 'zcode.cjs');
    if (!fs.existsSync(entry)) throw new Error(`zcode 入口不存在：${entry}（先按 README 构建）`);
    return { command: process.execPath, entry: [entry] };
  }
  const root = npmGlobalRoot();
  if (!root) throw new Error('拿不到 npm 全局根目录（npm root -g 失败）');
  const candidates =
    arm === 'codex'
      ? [path.join(root, '@openai', 'codex', 'bin', 'codex.js')]
      : [
          path.join(root, '@deepseek-ai', 'dsh', 'lib', 'bin.js'),
          path.join(root, '@deepseek-ai', 'dsh', 'bin', 'dsh.js'),
        ];
  const found = candidates.find((p) => fs.existsSync(p));
  if (!found) throw new Error(`${arm} 入口不存在，候选：${candidates.join(' | ')}`);
  return { command: process.execPath, entry: [found] };
}

/** 快照 → 工作区（robocopy /MIR，排除 target 以保住增量编译产物）。 */
function mirror(pristine, ws) {
  fs.mkdirSync(ws, { recursive: true });
  const res = sh('robocopy', [
    pristine,
    ws,
    '/MIR',
    '/XD',
    path.join(ws, 'target'),
    path.join(ws, 'src-tauri', 'target'),
    '/COPY:DAT',
    '/NFL',
    '/NDL',
    '/NJH',
    '/NJS',
    '/NP',
  ]);
  // robocopy 用位掩码：0-7 是成功（有拷贝/有跳过），>=8 才是错误。
  if (res.code < 0 || res.code > 7) throw new Error(`robocopy 失败 code=${res.code}：${tail(res.out + res.err)}`);
}

function applyFixtures(task, ws) {
  for (const fixture of task.fixtures ?? []) {
    const from = path.join(ROOT, fixture.from);
    const to = path.join(ws, fixture.to);
    fs.mkdirSync(path.dirname(to), { recursive: true });
    fs.copyFileSync(from, to);
  }
}

function injectAgentsNote(ws) {
  const file = path.join(ws, 'AGENTS.md');
  let text = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : '';
  if (text.includes(AGENTS_MARKER)) return;
  if (text && !text.endsWith('\n')) text += '\n';
  fs.writeFileSync(file, text + AGENTS_NOTE);
}

/**
 * 快照目录按需重建。
 *
 * 为什么不能只在启动时建一次：`%TEMP%` 下只有读取、没有写入的目录会被外部磁盘清理
 * 判定为陈旧并删掉（实测 dsh 臂跑到第 8 题时 `pristine/` 连同已完成题的工作区一起消失，
 * robocopy 报 `code=16 源目录不存在`）。每题准备前确认存活，删了就重建，几秒钟的事。
 */
function ensurePristine(ctx) {
  if (fs.existsSync(path.join(ctx.pristine, 'package.json'))) return;
  fs.rmSync(ctx.pristine, { recursive: true, force: true });
  git(ROOT, ['worktree', 'prune']);
  const add = git(ROOT, ['worktree', 'add', '--detach', ctx.pristine, ctx.baseCommit]);
  if (add.code !== 0) throw new Error(`创建快照 worktree 失败：${tail(add.out + add.err)}`);
  fs.rmSync(path.join(ctx.pristine, '.git'), { force: true });
  console.log(`[cmp] 快照目录已重建 ${ctx.pristine}`);
}

/**
 * 准备一题的工作区。返回 { ok, reason, ws, tree }。
 * 步骤顺序与 Rust `prepare_workspace` 一致：先 git init 再打补丁，否则 `git apply`
 * 会向上发现主仓库，把补丁落到仓库根。
 */
function prepareTask(task, ctx) {
  const ws = path.join(ctx.tmpRoot, 'ws', `${task.id}`);
  ensurePristine(ctx);
  fs.rmSync(path.join(ws, '.git'), { recursive: true, force: true });
  mirror(ctx.pristine, ws);

  const init = git(ws, ['init', '-q']);
  if (init.code !== 0) return { ok: false, reason: `git init 失败：${tail(init.out + init.err)}`, ws };

  if (task.patch) {
    const patch = path.join(ROOT, task.patch);
    if (!fs.existsSync(patch)) return { ok: false, reason: `补丁缺失：${task.patch}`, ws };
    const check = git(ws, ['apply', '--check', patch]);
    if (check.code !== 0) return { ok: false, reason: `补丁无法应用：${tail(check.out + check.err)}`, ws };
    const apply = git(ws, ['apply', patch]);
    if (apply.code !== 0) return { ok: false, reason: `补丁应用失败：${tail(apply.out + apply.err)}`, ws };
  }

  applyFixtures(task, ws);
  injectAgentsNote(ws);

  const add = git(ws, ['add', '-A']);
  if (add.code !== 0) return { ok: false, reason: `git add 失败：${tail(add.out + add.err)}`, ws };
  const commit = git(ws, ['commit', '-qm', 'task base']);
  if (commit.code !== 0) return { ok: false, reason: `基线提交失败：${tail(commit.out + commit.err)}`, ws };

  // 预热与 Rust 侧一致：编进 agent 的默认产物目录 `src-tauri/target`。
  // 不预热的话 codex/dsh 要自己冷编译，墙钟差一大截，对照就不是 harness 差距了。
  const warmStarted = Date.now();
  const manifest = path.join(ws, 'src-tauri', 'Cargo.toml');
  const warm = fs.existsSync(manifest)
    ? sh('cargo', ['test', '--manifest-path', manifest, '--lib', '--offline', '--no-run'], {
        cwd: ws,
        timeoutMs: 1200000,
      })
    : { code: 0, out: '', err: '' };
  const warmup = { ok: warm.code === 0, ms: Date.now() - warmStarted };
  if (!warmup.ok && task.warmup_compile !== false) {
    return { ok: false, reason: `预热失败：${tail(warm.out + warm.err, 300)}`, ws };
  }

  const listed = git(ws, ['ls-files', '-s']);
  if (listed.code !== 0) return { ok: false, reason: `ls-files 失败：${tail(listed.out + listed.err)}`, ws };
  // 与 Rust 侧同一算法：CRLF→LF 后对 `git ls-files -s` 全文取 sha256。
  const tree = sha256(listed.out.replace(/\r\n/g, '\n'));
  return { ok: true, ws, tree, warmup };
}

function runJudge(task, ws) {
  // `pwsh -File` 的输出编码跟随控制台（中文系统为 GBK），Node 按 UTF-8 解会变成乱码，
  // 判据原因就读不出来了。用 -Command 包一层，显式把控制台输出改成 UTF-8。
  const script = path.resolve(JUDGE_SCRIPT).replace(/'/g, "''");
  const command =
    `[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ` +
    `& '${script}' -Task '${task.judge.replace(/'/g, "''")}' -Root '${ws.replace(/'/g, "''")}'`;
  const res = sh('pwsh', ['-NoProfile', '-NonInteractive', '-Command', command], {
    timeoutMs: 1800000,
  });
  const text = `${res.out}\n${res.err}`;
  const pass = res.code === 0 && text.includes(PASS_MARKER);
  const tampered = TAMPER_MARKERS.some((marker) => text.includes(marker));
  const detail = text
    .split(/\r?\n/)
    .filter((line) => line.startsWith('REPO-CHECK:'))
    .join(' | ');
  // 判据全文必须留档：`REPO-CHECK: FAIL cargo 失败: ...` 只是结论，真正的原因在
  // 后面那几行 cargo 输出里。只留结论行会把「编译失败 / 依赖缺失 / 超时」糊成一句话。
  return { pass, tampered, detail: detail || tail(text, 400), ms: res.ms, code: res.code, text };
}

// 判据副本不带的目录：构建产物与前端依赖，判据会自己重新编译，不需要也不该继承。
const JUDGE_COPY_EXCLUDES = ['target', 'src-tauri\\target', 'node_modules'];

/**
 * 判据**不在 agent 的工作区里跑**，先复制一份只带数据、不带 ACL 的副本。
 *
 * 三个理由，前两个是实测：
 * 1. dsh 的 Windows 沙箱会给工作区根目录写 `Everyone: Deny DeleteSubdirectoriesAndFiles`，
 *    继承到 `target` 后 cargo 构建脚本删除产物时报 `os error 5`，判据会假红——那是沙箱差异，
 *    不是模型答错，不能算进能力分母。
 * 2. agent 也无法预置 `target/` 里的构建产物骗判据，隔离掉一类作弊面。
 * 3. `robocopy /COPY:DAT` 只复制数据/属性/时间，不复制 DACL/SACL，副本 ACL 来自新父目录。
 */
function judgeOnCopy(ws, ctx, task, run, phase) {
  const dst = path.join(ctx.tmpRoot, 'judge', `${task.id}-r${run}-${phase}`);
  fs.rmSync(dst, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  const copy = sh(
    'robocopy',
    [ws, dst, '/MIR', '/COPY:DAT', '/XD', ...JUDGE_COPY_EXCLUDES, '/NFL', '/NDL', '/NJH', '/NJS', '/NP'],
    { timeoutMs: 600000 },
  );
  // robocopy 0-7 是成功（含「有文件被复制」），>=8 才是失败。
  if (copy.code >= 8) {
    return {
      pass: false,
      tampered: false,
      detail: `REPO-CHECK: FAIL 判据副本复制失败（robocopy ${copy.code}）`,
      ms: copy.ms,
      code: copy.code,
      text: `${copy.out}\n${copy.err}`,
      copy_ms: copy.ms,
    };
  }
  const out = runJudge(task, dst);
  out.copy_ms = copy.ms;
  // 判据全文已随返回值落盘，副本用完即删：一份副本 2~4GB（含判据自己的编译产物），
  // 12 题累积会把盘撑满——上一轮 codex 臂就是被 `os error 112` 打断的。
  fs.rmSync(dst, { recursive: true, force: true, maxRetries: 3 });
  return out;
}

/**
 * 拉评测子进程用的干净环境。
 *
 * `CARGO_TARGET_DIR` 只服务于 harness 自身的编译（避免污染 src-tauri/target），
 * 绝不能传给被测 agent：agent 在沙箱内跑 `cargo` 时会把产物写到工作区之外，
 * 写入被拒后只能反复尝试重定向 target，白烧轮次（见 2026-11-16 的
 * phase2-shaniu-fix-compare.md：now-ms-exit 全场在绕行，最终零改动）。
 * 预热本身也依赖 agent 的默认目录 `src-tauri/target`，两处必须一致。
 */
function childEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  delete env.CARGO_TARGET_DIR;
  return env;
}

/** 起一个 agent 进程；到点用 taskkill 杀整棵树，避免子进程拖住驱动器。 */
function runAgent(command, args, { cwd, env, timeoutMs }) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    let killed = false;
    child.stdout.on('data', (chunk) => {
      out += chunk;
    });
    child.stderr.on('data', (chunk) => {
      err += chunk;
    });
    const timer = setTimeout(() => {
      killed = true;
      spawnSync('taskkill', ['/T', '/F', '/PID', String(child.pid)], { windowsHide: true });
    }, timeoutMs);
    child.on('error', (error) => {
      clearTimeout(timer);
      resolve({ code: -1, out, err: `${err}${error.message}`, killed });
    });
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ code: code ?? -1, out, err, killed });
    });
  });
}

/** 从 NDJSON 里取用量。三方字段名不同，统一成 input_total / cached / output。 */
function parseUsage(arm, ndjson) {
  const lines = ndjson.split(/\r?\n/).filter((line) => line.trim().startsWith('{'));
  const usage = { input_total: 0, cached: 0, output: 0, turns: 0, source: 'none' };
  for (const line of lines) {
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    if (arm === 'codex' && event.type === 'turn.completed' && event.usage) {
      usage.input_total += Number(event.usage.input_tokens ?? 0);
      usage.cached += Number(event.usage.cached_input_tokens ?? 0);
      usage.output += Number(event.usage.output_tokens ?? 0);
      usage.turns += 1;
      usage.source = 'codex.turn.completed';
    }
    // dsh：status/step_end 的 usage 是增量，按 step 累加；cacheRead 属于输入侧。
    if (arm === 'dsh' && event.type === 'status' && event.phase === 'step_end' && event.usage) {
      const u = event.usage;
      usage.input_total += Number(u.inputTokens ?? 0) + Number(u.cacheReadTokens ?? 0) + Number(u.cacheWriteTokens ?? 0);
      usage.cached += Number(u.cacheReadTokens ?? 0);
      usage.output += Number(u.outputTokens ?? 0);
      usage.turns += 1;
      usage.source = 'dsh.status.step_end';
    }
    // zcode：终结事件 `result.usage` 是整轮累计值（含 modelRequestCount），优先用它；
    // 单步的 `model_request_completed` 只用来兜底数模型请求次数。
    if (arm === 'zcode') {
      if (event.type === 'result' && event.usage) {
        const u = event.usage;
        usage.input_total = Number(u.inputTokens ?? 0) + Number(u.cacheReadTokens ?? 0) + Number(u.cacheWriteTokens ?? 0);
        usage.cached = Number(u.cacheReadTokens ?? 0);
        usage.output = Number(u.outputTokens ?? 0);
        usage.turns = Number(u.modelRequestCount ?? 0);
        usage.source = 'zcode.result';
      } else if (event.payload?.type === 'model_request_completed' && usage.source !== 'zcode.result') {
        usage.turns += 1;
        usage.source = 'zcode.model_request_completed';
      }
    }
  }
  if (usage.turns === 0) return null;
  return usage;
}

function diffStat(task, ws) {
  const numstat = git(ws, ['diff', '--numstat', 'HEAD']);
  let added = 0;
  let deleted = 0;
  const files = [];
  for (const line of numstat.out.split(/\r?\n/)) {
    const cols = line.split('\t');
    if (cols.length < 3) continue;
    const a = Number.parseInt(cols[0], 10) || 0;
    const d = Number.parseInt(cols[1], 10) || 0;
    added += a;
    deleted += d;
    if (files.length < 12) files.push(`${cols[2]} +${a}/-${d}`);
  }
  const status = git(ws, ['status', '--porcelain']);
  const untrackedPaths = status.out
    .split(/\r?\n/)
    .filter((line) => line.startsWith('??'))
    .map((line) => line.slice(3).trim())
    .filter(Boolean);
  const untracked = untrackedPaths.length;
  // 触碰禁令路径要同时看「改的」和「新加的」：只改不删和新拉一个禁用文件一样越界。
  const allPaths = [...files.map((entry) => entry.split(' ')[0]), ...untrackedPaths];
  const forbidHit = allPaths.some((path) =>
    (task.forbid_paths ?? []).some((prefix) => path.startsWith(prefix.replace(/\\/g, '/'))),
  );
  return {
    files: files.length,
    added,
    deleted,
    untracked,
    untracked_files: untrackedPaths.slice(0, 10),
    forbid_hit: forbidHit,
    summary: files.join('；'),
  };
}

/** 判据日志的结论：只认 `REPO-CHECK: PASS`，且不能同时出现 `REPO-CHECK: FAIL`。 */
function readJudgeLog(file) {
  if (!fs.existsSync(file)) return { pass: false, text: '', detail: '' };
  const text = fs.readFileSync(file, 'utf8');
  const pass = text.includes(PASS_MARKER) && !text.includes('REPO-CHECK: FAIL');
  const detail = text
    .split(/\r?\n/)
    .filter((line) => line.startsWith('REPO-CHECK:'))
    .join(' | ');
  return { pass, text, detail: detail || tail(text, 400) };
}

/** 每题完成一行进度：活着还是又断了，看日志最后一行就知道，不用等两小时。 */
function logProgress(record, ctx) {
  const usage = record.usage
    ? `in=${record.usage.input_total} out=${record.usage.output} turns=${record.usage.turns}`
    : 'usage=N/A';
  const diff = record.diff ? `diff=${record.diff.files}文件 +${record.diff.added}/-${record.diff.deleted}` : 'diff=N/A';
  const line =
    `[cmp] done ${record.task} ${record.status} wall=${record.wall_ms ?? '-'}ms ${usage} ${diff}` +
    `${record.resumed ? '（续跑复用）' : ''} @ ${new Date().toISOString()}`;
  console.log(line);
  // 同时落一份进度文件：stdout 若被外层包装截流，仍能从文件判断进度。
  try {
    fs.appendFileSync(path.join(ctx.outDir, 'progress.log'), `${line}\n`);
  } catch {
    // 进度文件只是可观测信号，写不进去不影响跑题。
  }
}

/** 开跑前把「预计题数 × 单题预算」写清楚，中断时才能判断是超预算还是挂死。 */
function budgetLine(tasks) {
  const cap = (task) => task.timeout_secs + 60;
  const sum = tasks.reduce((acc, task) => acc + cap(task), 0);
  const max = tasks.reduce((acc, task) => Math.max(acc, cap(task)), 0);
  return (
    `[cmp] 预算：${tasks.length} 题 × 单题 agent 段上限 ${max}s（逐题 timeout_secs+60s；` +
    `明细 ${tasks.map((task) => `${task.id}=${cap(task)}s`).join(',')}）；` +
    `agent 段合计上限 ${sum}s ≈ ${(sum / 3600).toFixed(1)}h；判据另计（复制+编译，实测 90~300s/题）`
  );
}

/** 旧工作区用完即删：单题 3~5GB，留到整轮结束会把下一题的镜像挤爆（上一轮就是这么断的）。 */
function dropOldWorkspace(task, ctx) {
  if (!ctx.resumeWs) return;
  const ws = path.join(ctx.resumeWs, task.id);
  if (!fs.existsSync(ws)) return;
  try {
    fs.rmSync(ws, { recursive: true, force: true, maxRetries: 3 });
    console.log(`[cmp] 回收旧工作区 ${task.id}`);
  } catch (error) {
    console.log(`[cmp] 旧工作区回收失败 ${task.id}：${error.message}`);
  }
}

/** 已有同名原始文件先归档再写：重跑不能把上一轮的证据覆盖掉。 */
function archiveRaw(file, ctx) {
  if (!fs.existsSync(file)) return;
  const dir = path.join(ctx.outDir, 'raw', 'archive');
  fs.mkdirSync(dir, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  fs.renameSync(file, path.join(dir, `${path.basename(file)}.${stamp}`));
}

/** 上一轮 summary 里同一题的记录：续跑时旧工作区可能已被外部清理删掉，元数据不能跟着丢。 */
function previousRecord(ctx, task, run) {
  const file = path.join(ctx.outDir, 'summary.json');
  if (!fs.existsSync(file)) return null;
  try {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    return (data.records ?? []).find((item) => item.task === task.id && (item.run ?? 1) === run) ?? null;
  } catch {
    return null;
  }
}

/**
 * 续跑：复用已落盘的 ndjson / judge.log，不重跑 agent（重跑会覆盖事件流）。
 *
 * 判据结论从 judge.log 读，因为驱动器没把判据退出码写进文件。但「日志说红」有两种来源：
 * 真失败，和判据自己跑挂了（上一轮第 7 题的红就是 `磁盘空间不足 (os error 112)`）。
 * 旧工作区还在就重判一次——只重跑判据、不重跑 agent，结果写到 `.resume.judge.log`，
 * 原 judge.log 保留作证据。
 */
function resumeRecord(task, run, ctx) {
  const raw = path.join(ctx.outDir, 'raw');
  const ndjsonPath = path.join(raw, `${ctx.armName}-${task.id}-r${run}.ndjson`);
  const judgePath = path.join(raw, `${ctx.armName}-${task.id}-r${run}.judge.log`);
  if (!fs.existsSync(ndjsonPath) || !fs.existsSync(judgePath)) return null;

  const record = baseRecord(task, run, ctx);
  record.resumed = true;
  record.usage = parseUsage(ctx.armName, fs.readFileSync(ndjsonPath, 'utf8'));
  // 旧工作区已被 agent 改过，基线指纹无从重建：该题指纹记 unknown，不参与闸门判定。
  record.tree = 'unknown';
  record.precondition_red = !readJudgeLog(path.join(raw, `${ctx.armName}-${task.id}-r${run}.pre.judge.log`)).pass;

  const judge = readJudgeLog(judgePath);
  record.judge_pass = judge.pass;
  record.judge_detail = judge.detail;
  record.tampered = TAMPER_MARKERS.some((marker) => judge.text.includes(marker));

  const ws = ctx.resumeWs ? path.join(ctx.resumeWs, task.id) : '';
  const hasWs = Boolean(ws) && fs.existsSync(path.join(ws, '.git'));

  if (!judge.pass && !record.tampered && hasWs) {
    const again = judgeOnCopy(ws, ctx, task, run, 'resume');
    fs.writeFileSync(path.join(raw, `${ctx.armName}-${task.id}-r${run}.resume.judge.log`), again.text ?? '');
    record.judge_pass = again.pass;
    record.judge_detail = again.detail;
    record.judge_ms = again.ms + (again.copy_ms ?? 0);
    record.tampered = again.tampered;
    record.rejudged = true;
    if (!again.pass) record.judge_tail = tail(again.text ?? '', 1200);
  }

  // 判据红且日志里留着「判据自己跑挂了」的痕迹（磁盘满、目录被外部清掉、副本复制失败）：
  // 这不是可用结果，返回 null 让它重跑。重跑前旧 ndjson/judge.log 会归档到 raw/archive/。
  if (!record.tampered && !record.judge_pass && ENV_FAIL_MARKERS.some((marker) => judge.text.includes(marker))) {
    console.log(`[cmp] 判据红不可归因（环境故障痕迹），重跑：${task.id}`);
    return null;
  }

  if (hasWs) record.diff = diffStat(task, ws);
  // 旧工作区被外部清理掉时，diff / 墙钟 / token 只能沿用上一轮 summary，
  // 否则一次续跑会把已入库的改动面全抹成 N/A。
  const prev = previousRecord(ctx, task, run);
  if (prev) {
    record.diff ??= prev.diff ?? null;
    record.agent_ms ??= prev.agent_ms ?? null;
    record.judge_ms ??= prev.judge_ms ?? null;
    record.wall_ms ??= prev.wall_ms ?? null;
    record.timed_out ??= prev.timed_out ?? null;
    record.usage ??= prev.usage ?? null;
  }
  if (record.tampered) {
    record.status = 'INVALID';
    record.invalid_reason = '判据命中守卫失败（测试区/判据被动过）';
  } else if (record.judge_pass) {
    record.status = 'PASS';
  } else {
    record.status = 'FAIL';
    if (!hasWs) record.invalid_reason = '旧工作区已不存在，判据红无法复核';
  }
  return record;
}

function armCommand(arm, model, ws, prompt) {
  if (arm === 'codex') {
    return {
      args: [
        'exec',
        '-C',
        ws,
        '--skip-git-repo-check',
        '-s',
        'workspace-write',
        '--json',
        '-m',
        model,
        '-c',
        'model_provider="deepseek"',
        '-c',
        'model_providers.deepseek.name="DeepSeek"',
        '-c',
        'model_providers.deepseek.base_url="https://api.deepseek.com/v1"',
        '-c',
        'model_providers.deepseek.env_key="DEEPSEEK_API_KEY"',
        '-c',
        'model_providers.deepseek.wire_api="responses"',
        prompt,
      ],
    };
  }
  if (arm === 'dsh') {
    return {
      args: [
        '--profile',
        'headless',
        '--json',
        '--permission-mode',
        'workspace-write',
        '--approval-policy',
        'never',
        prompt,
      ],
    };
  }
  if (arm === 'zcode') {
    // zcode 没有 --model / --provider 全局参数：模型与端点在个人 provider 配置里，
    // 由 ZCODE_PERSONAL_PROVIDER_CONFIG_FILE 指过去（见 README「zcode 臂」）。
    // `--mode yolo` 是它的「自动放行 + 不询问」，与另两方的 workspace-write 同口径。
    return {
      args: [
        '-p',
        prompt,
        '--cwd',
        ws,
        '--mode',
        'yolo',
        '--output-format',
        'stream-json',
        '--no-browser',
      ],
    };
  }
  throw new Error(`arm ${arm} 不走 CLI 路径`);
}

function apiKey() {
  const key = process.env.DeepSeekAPI ?? process.env.DEEPSEEK_API_KEY;
  if (!key) throw new Error('缺少 DeepSeekAPI / DEEPSEEK_API_KEY');
  return key;
}

/**
 * zcode 的模型/端点全靠「个人 provider 配置」注入：它没有 `--model` / `--base-url` 参数，
 * 内置目录里 DeepSeek 走的是 `anthropic-messages`，不是我们统一的 Responses。
 * 配置写在 `target/`（已忽略入库），key 不进仓库、不进日志。
 */
function zcodeEnv(key) {
  const dir = path.join(ROOT, 'target', 'zcode-personal');
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, 'provider_config.json');
  const config = {
    schemaVersion: 1,
    config: {
      providerOrder: ['deepseek-personal'],
      providerConfigRules: {
        providerRules: [
          {
            providerId: 'deepseek-personal',
            providerName: 'DeepSeek Personal',
            enabled: true,
            config: {
              group: 'standard-personal',
              access: { type: 'api-key', apiKey: key },
              api: { type: 'openai-responses', baseUrl: 'https://api.deepseek.com/v1' },
              personalModelIds: ['deepseek-flash'],
            },
          },
        ],
      },
      modelConfigRules: { providerModelRules: [], manualProviderModelRules: [] },
      defaultModelSelection: { providerId: 'deepseek-personal', modelId: 'deepseek-flash' },
    },
  };
  fs.writeFileSync(file, JSON.stringify(config, null, 2), 'utf8');
  return {
    ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: path.join(ZCODE_REPO, 'config', 'provider', 'zcode-builtin.json'),
    ZCODE_PERSONAL_PROVIDER_CONFIG_FILE: file,
    // 关掉遥测/自动升级这类会联网写盘的东西，避免污染墙钟与工作区。
    ZCODE_TELEMETRY_DISABLED: '1',
    NO_COLOR: '1',
  };
}

async function runCliArm(task, run, ctx) {
  const prepared = prepareTask(task, ctx);
  const record = baseRecord(task, run, ctx);
  record.tree = prepared.tree ?? 'unknown';
  if (!prepared.ok) {
    record.status = 'INVALID';
    record.invalid_reason = prepared.reason;
    return record;
  }
  // 工作区指纹闸门：三方必须在同一棵树上起跑，否则差异里混进了准备逻辑的噪声。
  const expected = ctx.expectTree.get(task.id);
  if (expected && expected !== record.tree) {
    record.status = 'INVALID';
    record.invalid_reason = `工作区指纹不一致：期望 ${expected} 实得 ${record.tree}`;
    return record;
  }

  // 前置判据必须红：不红说明补丁没打上或判据不区分，该题不进分母。
  const pre = judgeOnCopy(prepared.ws, ctx, task, run, 'pre');
  const preLog = path.join(ctx.outDir, 'raw', `${ctx.armName}-${task.id}-r${run}.pre.judge.log`);
  archiveRaw(preLog, ctx);
  fs.writeFileSync(preLog, pre.text ?? '');
  record.precondition_red = !pre.pass;
  record.precondition_detail = pre.detail;
  if (pre.tampered) {
    record.status = 'INVALID';
    record.tampered = true;
    record.invalid_reason = '前置判据命中守卫失败（测试区/判据被动过）';
    return record;
  }
  if (pre.pass) {
    record.status = 'INVALID';
    record.invalid_reason = '前置判据为绿：补丁未生效或判据不区分';
    return record;
  }
  if (ctx.prepareOnly) {
    record.status = 'PREPARED';
    return record;
  }

  const { command, entry } = ctx.arm;
  const spec = armCommand(ctx.armName, ctx.model, prepared.ws, task.prompt);
  const env = childEnv({
    DEEPSEEK_API_KEY: ctx.key,
    DSH_HOME: path.join(ROOT, 'target', 'dsh-home'),
  });
  if (ctx.armName === 'zcode') {
    Object.assign(env, zcodeEnv(ctx.key));
  }
  const started = Date.now();
  const agent = await runAgent(command, [...entry, ...spec.args], {
    cwd: prepared.ws,
    env,
    timeoutMs: (task.timeout_secs + 60) * 1000,
  });
  record.agent_ms = Date.now() - started;
  record.timed_out = agent.killed;
  const ndjsonPath = path.join(ctx.outDir, 'raw', `${ctx.armName}-${task.id}-r${run}.ndjson`);
  archiveRaw(ndjsonPath, ctx);
  fs.writeFileSync(ndjsonPath, agent.out + agent.err);

  const usage = parseUsage(ctx.armName, agent.out);
  record.usage = usage;
  record.exit_code = agent.code;

  const post = judgeOnCopy(prepared.ws, ctx, task, run, 'post');
  const postLog = path.join(ctx.outDir, 'raw', `${ctx.armName}-${task.id}-r${run}.judge.log`);
  archiveRaw(postLog, ctx);
  fs.writeFileSync(postLog, post.text ?? '');
  record.judge_pass = post.pass;
  record.judge_detail = post.detail;
  record.judge_copy_ms = post.copy_ms ?? null;
  // 失败时把 cargo/脚本的真实尾巴也带上，否则 summary 里只剩一句「cargo 失败」。
  if (!post.pass) record.judge_tail = tail(post.text ?? '', 1200);
  record.judge_ms = post.ms + (post.copy_ms ?? 0);
  record.tampered = post.tampered;
  record.diff = diffStat(task, prepared.ws);
  record.wall_ms = record.agent_ms + record.judge_ms;

  if (post.tampered) {
    record.status = 'INVALID';
    record.invalid_reason = '判据命中守卫失败（测试区/判据被动过）';
  } else if (post.pass) {
    record.status = 'PASS';
  } else if (agent.killed) {
    record.status = 'TIMEOUT';
  } else {
    record.status = 'FAIL';
  }
  return record;
}

function baseRecord(task, run, ctx) {
  return {
    arm: ctx.armName,
    task: task.id,
    task_type: task.kind,
    run,
    model: ctx.model,
    protocol: 'responses',
    status: 'UNKNOWN',
    precondition_red: null,
    judge_pass: null,
    tampered: false,
    timed_out: false,
    agent_ms: null,
    judge_ms: null,
    wall_ms: null,
    diff: null,
    usage: null,
    tree: 'unknown',
  };
}

/** shaniu 臂：转调 Rust 运行器（准备/判据/落盘都由它保持一致），只读回它的报告。 */
function runShaniuArm(tasks, ctx) {
  const before = new Set(
    fs.existsSync(path.join(ROOT, 'evals', 'results'))
      ? fs.readdirSync(path.join(ROOT, 'evals', 'results')).filter((f) => f.endsWith('-repo.json'))
      : [],
  );
  const logFile = path.join(ctx.outDir, 'raw', 'shaniu-run.log');
  const res = sh(
    'cargo',
    [
      'test',
      '--manifest-path',
      path.join(ROOT, 'src-tauri', 'Cargo.toml'),
      '--test',
      'evals_repo',
      'evals_repo_v0',
      '--offline',
      '--',
      '--ignored',
      '--exact',
      '--nocapture',
    ],
    {
      cwd: ROOT,
      timeoutMs: 6 * 60 * 60 * 1000,
      env: childEnv({
        SHANIU_TEST_API_KEY: ctx.key,
        SHANIU_TEST_PROVIDER: `openai-${ctx.protocol}`,
        SHANIU_TEST_BASE_URL: 'https://api.deepseek.com/v1',
        SHANIU_TEST_MODEL: ctx.model,
        SHANIU_EVAL_REPEATS: String(ctx.repeats),
        SHANIU_EVAL_TASK_FILTER: tasks.map((t) => t.id).join(','),
        SHANIU_EVAL_RUN_LOG: logFile,
      }),
    },
  );
  fs.writeFileSync(path.join(ctx.outDir, 'raw', 'shaniu-stdout.log'), res.out + res.err);

  const dir = path.join(ROOT, 'evals', 'results');
  const fresh = fs
    .readdirSync(dir)
    .filter((f) => f.endsWith('-repo.json') && !before.has(f))
    .map((f) => path.join(dir, f))
    .sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs);
  if (!fresh.length) throw new Error(`shaniu 臂没有产出新报告（cargo exit=${res.code}）`);

  const report = JSON.parse(fs.readFileSync(fresh[0], 'utf8'));
  const trees = new Map();
  if (fs.existsSync(logFile)) {
    for (const line of fs.readFileSync(logFile, 'utf8').split(/\r?\n/)) {
      const m = line.match(/\[prep\] task=(\S+) base=\S+ tree=(\w+)/);
      if (m) trees.set(m[1], m[2]);
    }
  }
  return (report.tasks ?? []).map((item) => {
    const detail = item.detail ?? '';
    // 守卫失败（改测试区/删断言）一律 invalid：这是反作弊口径，不是能力失败。
    const tampered = TAMPER_MARKERS.some((marker) => detail.includes(marker));
    const diffText = item.diff_summary ?? '';
    const diffMatch = diffText.match(/共(\d+)文件 \+(\d+)\/-(\d+)；未跟踪(\d+)/);
    const record = {
      arm: 'shaniu',
      task: item.id,
      task_type: item.type,
      run: 1,
      model: ctx.model,
      protocol: ctx.protocol,
      status: item.success ? 'PASS' : item.invalid || tampered ? 'INVALID' : item.timeout ? 'TIMEOUT' : 'FAIL',
      invalid_reason: tampered ? '判据命中守卫失败（测试区/判据被动过）' : (item.invalid_reason ?? null),
      precondition_red: item.precondition_red ?? null,
      judge_pass: item.success ?? null,
      tampered,
      timed_out: Boolean(item.timeout),
      agent_ms: item.agent_ms ?? null,
      judge_ms: item.check_ms ?? null,
      wall_ms: (item.agent_ms ?? 0) + (item.check_ms ?? 0),
      diff: {
        summary: diffText,
        files: item.files_changed ?? 0,
        added: diffMatch ? Number(diffMatch[2]) : null,
        deleted: diffMatch ? Number(diffMatch[3]) : null,
        untracked: diffMatch ? Number(diffMatch[4]) : null,
        forbid_hit: Boolean(item.forbidden_touched),
      },
      usage:
        item.usage_turns > 0
          ? {
              input_total: item.input_tokens ?? 0,
              cached: item.cached_input_tokens ?? 0,
              output: item.output_tokens ?? 0,
              turns: item.usage_turns,
              source: 'shaniu.TokenUsage',
            }
          : null,
      tree: trees.get(item.id) ?? 'unknown',
      detail,
    };
    // 工作区指纹闸门（与 CLI 臂同一口径）。
    const expected = ctx.expectTree.get(item.id);
    if (expected && record.tree !== expected) {
      record.status = 'INVALID';
      record.invalid_reason = `工作区指纹不一致：期望 ${expected} 实得 ${record.tree}`;
    }
    return record;
  });
}

async function main() {
  const args = parseArgs(process.argv);
  const spec = JSON.parse(fs.readFileSync(TASKS_FILE, 'utf8'));
  const selected = args.tasks.length ? spec.tasks.filter((t) => args.tasks.includes(t.id)) : spec.tasks;
  if (!selected.length) throw new Error(`题集里没有匹配的任务：${args.tasks.join(',')}`);

  const runId = `${Date.now()}-${process.pid}`;
  const outDir = path.resolve(args.out || path.join(ROOT, 'evals', 'results-compare', `${runId}-${args.arm}`));
  fs.mkdirSync(path.join(outDir, 'raw'), { recursive: true });
  const tmpRoot = path.join(os.tmpdir(), `shaniu-cmp-${runId}`);
  fs.mkdirSync(tmpRoot, { recursive: true });

  const ctx = {
    armName: args.arm,
    model: args.model,
    // SHANIU 默认走 chat：实测它的 Responses 路径把普通文本消息序列化成 `text` 字段，
    // DeepSeek 直接忽略，模型收不到任务内容（见 report 的「协议差异」一节）。
    // codex/dsh 本版只支持 Responses，所以协议差异必须在报告里明写，不能当成 harness 差距。
    protocol: args.protocol || (args.arm === 'shaniu' ? 'chat' : 'responses'),
    repeats: args.repeats,
    outDir,
    tmpRoot,
    prepareOnly: args.prepareOnly,
    expectTree: args.expectTree,
    resume: args.resume,
    resumeWs: args.resumeWs ? path.resolve(args.resumeWs) : '',
    baseCommit: spec.base_commit,
    key: apiKey(),
    pristine: '',
    arm: { command: '', entry: [] },
  };

  // shaniu 不需要自己准备快照：Rust 运行器内部用 read-tree 物化。
  if (args.arm !== 'shaniu') {
    ctx.arm = resolveArm(args.arm);
    ctx.pristine = path.join(tmpRoot, 'pristine');
    ensurePristine(ctx);
  }

  console.log(`[cmp] arm=${args.arm} model=${args.model} 题数=${selected.length} 输出=${outDir}`);
  if (args.resume) console.log(`[cmp] 续跑：复用 ${outDir} 下已有 ndjson/judge.log，不重跑已出结果的题`);
  const freeBefore = diskFreeGb();
  const records =
    args.arm === 'shaniu' ? runShaniuArm(selected, ctx) : await runCliArmLoop(selected, ctx);

  fs.writeFileSync(path.join(outDir, 'summary.json'), JSON.stringify({ runId, args, records }, null, 2));
  for (const r of records) {
    const usage = r.usage ? `in=${r.usage.input_total} out=${r.usage.output}` : 'usage=N/A';
    console.log(
      `[cmp] ${r.task.padEnd(34)} ${String(r.status).padEnd(8)} wall=${r.wall_ms ?? '-'}ms ${usage} diff=${
        r.diff?.files ?? '-'
      }文件`,
    );
  }
  const tally = new Map();
  for (const r of records) tally.set(r.status, (tally.get(r.status) ?? 0) + 1);
  console.log(
    `[cmp] 收尾：${records.length} 题 [${[...tally].map(([k, v]) => `${k}=${v}`).join(' ')}]，` +
      `复用 ${records.filter((r) => r.resumed).length} 题；C 盘可用 ${freeBefore}GB → ${diskFreeGb()}GB；输出=${outDir}`,
  );

  if (args.arm !== 'shaniu') {
    git(ROOT, ['worktree', 'prune']);
  }
  // 排障用：需要比对工作区内容时保留临时目录。
  if (!process.env.SHANIU_CMP_KEEP) {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
}

async function runCliArmLoop(tasks, ctx) {
  const byKey = new Map();
  const pending = [];
  // 第一遍：复用已出结果的题（续跑）。放在跑新题之前——顺带把旧工作区回收掉，
  // 给后面 5 题的镜像腾空间；上一轮断在镜像准备上，就是因为旧工作区还在占 24GB。
  for (const task of tasks) {
    for (let run = 1; run <= ctx.repeats; run += 1) {
      const reused = ctx.resume ? resumeRecord(task, run, ctx) : null;
      if (!reused) {
        pending.push({ task, run });
        continue;
      }
      logProgress(reused, ctx);
      byKey.set(`${task.id}#${run}`, reused);
      dropOldWorkspace(task, ctx);
    }
  }
  console.log(`[cmp] ${ctx.resume ? '续跑' : '开跑'}：待跑 ${pending.length} 题 / 复用 ${byKey.size} 题（合计 ${tasks.length * ctx.repeats}）`);
  if (pending.length) console.log(budgetLine(pending.map((item) => item.task)));

  for (const { task, run } of pending) {
    console.log(`[cmp] start ${task.id} r${run}`);
    const record = await runCliArm(task, run, ctx);
    logProgress(record, ctx);
    byKey.set(`${task.id}#${run}`, record);
  }

  // 按题集顺序归位：复用与实跑的记录混在一起时，summary 仍可逐题比对。
  const records = [];
  for (const task of tasks) {
    for (let run = 1; run <= ctx.repeats; run += 1) records.push(byKey.get(`${task.id}#${run}`));
  }
  return records;
}

main().catch((error) => {
  console.error(`[cmp] 失败：${error.message}`);
  console.error(`[cmp] 收尾：本轮中断 @ ${new Date().toISOString()}；已完成的结果已落盘，可用 --resume 续跑`);
  process.exit(1);
});

// 手动中断也要有收尾行：否则日志会像上一轮那样停在一个 start 上，看不出是断还是慢。
process.on('SIGINT', () => {
  console.error(`[cmp] 收尾：收到中断 @ ${new Date().toISOString()}；已完成的结果已落盘，可用 --resume 续跑`);
  process.exit(130);
});
