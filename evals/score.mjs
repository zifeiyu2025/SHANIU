#!/usr/bin/env node
// 评估基线回归门禁（docs/05 SH-028、docs/04 §6.5、AGENTS.md §5）。
//
// 只读三份报告做判定，不联网、不起 Agent。跑由
//   cargo test --test evals_baseline -- --ignored --nocapture
// 负责（默认 5 次，SHANIU_EVAL_REPEATS 可覆盖），本脚本负责"这次变更能不能合并"。
//
// **分母是当日对照，不是冻结基线**：端点漂移实测把整份基线抬升 32%/轮，
// 冻结点已经不是同一把尺子；拿它当分母，结论永远红且红得没有意义。
//   对照臂：SHANIU_EVAL_ROLE=control SHANIU_EVAL_ABLATE_AR6=1  → evals/results/*-control.json
//   实验臂：SHANIU_EVAL_ROLE=treatment（默认，条款常驻）      → evals/results/*-treatment.json
// 两臂必须同日、同端点、同模型、同参数（任务集/轮数上限/重复次数），否则拒绝下结论。
//
// 用法：
//   node evals/score.mjs <实验.json> <对照.json>   # 显式传两份（推荐；跑完会打印本次路径）
//   node evals/score.mjs <实验.json> --promote    # 写成该模型的历史参考点（仅参考，不参与判定）
//   node evals/score.mjs                          # 兜底：读 evals/latest.json 与 evals/control.json（旧产物）
//
// 拒绝下结论的硬条件（除下面三条判据外）：报告 valid=false、样本数与实际档案数不符、
// 两臂 pid 相同（同一个进程只可能是一种 arm）。
//
// 判据（相对变化，阈值 10% 不变）：
//   - success_rate：中位数下降 > 10% → 硬否决。
//   - avg_turns / avg_total_tokens：中位数上升 > 10% 且**每一次运行都越阈**才判红；
//     只有中位数越阈、运行间不一致 → 观察项，不阻断。
//
// 冻结基线 `evals/baseline.<model>.json` 只在最后打印为**历史参考**（以及漂移幅度），
// 不参与任何 pass/block 判定。

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const THRESHOLD = 0.1;
const MIN_RUNS = 5;
const promote = process.argv.includes('--promote');
// 显式传路径：`node evals/score.mjs <实验.json> [对照.json]`。
// 不再默认读"最新那份"——2026-11-09 的并发事故就是被"最新"掩盖的：
// 磁盘上的 latest.json 是另一个进程写的，报告自报的却是另一组数字。
const positional = process.argv.slice(2).filter((arg) => !arg.startsWith('--'));

function loadPath(file) {
  try {
    return JSON.parse(readFileSync(file, 'utf8'));
  } catch (e) {
    refuse(`读不到 ${file}：${e.message}`);
  }
}

function load(name, { required = true } = {}) {
  try {
    return JSON.parse(readFileSync(join(HERE, name), 'utf8'));
  } catch (e) {
    if (required) {
      console.error(`[evals] 读不到 ${name}：${e.message}`);
      process.exit(2);
    }
    return null;
  }
}

const latest = positional[0] ? loadPath(positional[0]) : load('latest.json');
if (promote) {
  const name = `baseline.${latest.model}.json`;
  writeFileSync(join(HERE, name), `${JSON.stringify(latest, null, 2)}\n`);
  console.log(`[evals] 已把 latest 写为历史参考点 ${name}（不参与判定）`);
  process.exit(0);
}

const control = positional[1] ? loadPath(positional[1]) : load('control.json');
const fmt = (v) => (typeof v === 'number' ? (Number.isInteger(v) ? `${v}` : v.toFixed(3)) : `${v}`);
const pct = (d) => `${d >= 0 ? '+' : ''}${(d * 100).toFixed(1)}%`;
const dateOf = (ms) => new Date(ms).toLocaleDateString('sv-SE');

function rel(a, b) {
  if (a === 0) return b === 0 ? 0 : Number.POSITIVE_INFINITY;
  return (b - a) / a;
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[mid - 1] + sorted[mid]) / 2 : sorted[mid];
}

function refuse(message) {
  console.error(`[evals] 不能下结论：${message}`);
  process.exit(2);
}

const cRuns = Array.isArray(control.runs) ? control.runs : [];
const tRuns = Array.isArray(latest.runs) ? latest.runs : [];
// `--exploratory`：允许每臂少于 5 次，用于"先量波动、再决定要不要加成本"的探索性一轮。
// 代价必须说清：样本越少，能探测的差异越大；结论只允许落在
// 「观察到增益 / 未观察到差异 / 不适用于该任务集」三者之一。
const exploratory = process.argv.includes('--exploratory');
if ((cRuns.length < MIN_RUNS || tRuns.length < MIN_RUNS) && !exploratory) {
  refuse(
    `样本不足（对照 ${cRuns.length} 次 / 实验 ${tRuns.length} 次），两臂各需 >= ${MIN_RUNS} 次。\n` +
      '      重跑：SHANIU_EVAL_REPEATS=5 ...；探索性对照可加 --exploratory',
  );
}
if (exploratory && (cRuns.length < MIN_RUNS || tRuns.length < MIN_RUNS)) {
  console.warn(
    `[evals] 探索模式：对照 ${cRuns.length} 次 / 实验 ${tRuns.length} 次（< ${MIN_RUNS}）。` +
      '结论只能写「观察到增益 / 未观察到差异 / 不适用于该任务集」，不得写「无效 / 证明没有」',
  );
}
// 对照轴：至少有一条不同（A-R6 或自愈闭环）。两条都相同 = 不是对照实验。
if (control.arm === latest.arm && control.repair_enabled === latest.repair_enabled) {
  refuse(
    `两臂 arm 与 repair_enabled 都相同（arm=${latest.arm}，repair=${fmt(latest.repair_enabled)}），` +
      '不是对照实验：自愈对照需带 SHANIU_EVAL_ABLATE_REPAIR=1',
  );
}
for (const key of ['model', 'base_url', 'max_turns_per_task', 'repeats', 'version']) {
  if (control[key] !== latest[key]) {
    refuse(`参数不一致：${key} 对照=${fmt(control[key])} 实验=${fmt(latest[key])}`);
  }
}
if (dateOf(control.epoch_ms) !== dateOf(latest.epoch_ms)) {
  refuse(
    `跨日对比无效：对照=${dateOf(control.epoch_ms)}，实验=${dateOf(latest.epoch_ms)}。` +
      '端点漂移按天变化，必须同日重跑两臂。',
  );
}
const ids = (report) => report.tasks.map((t) => t.id).sort().join(',');
if (ids(control) !== ids(latest)) {
  refuse('两臂任务集不一致');
}

// 自证校验：报告必须能回答"这份数字是谁跑的、跑满了没有"。
// 2026-11-09 的并发事故里，两边数字都"看起来正常"，唯一破绽是样本对不上。
for (const [label, report] of [
  ['实验', latest],
  ['对照', control],
]) {
  if (report.valid === false) {
    refuse(
      `${label}报告自证无效：样本数 ${report.samples?.expected} != 实际档案 ${report.samples?.actual_sessions}`,
    );
  }
  if (report.samples && report.samples.expected !== report.samples.actual_sessions) {
    refuse(
      `${label}报告样本数对不上（期望 ${report.samples.expected}，实际 ${report.samples.actual_sessions}）`,
    );
  }
  if (!report.process?.pid) {
    console.warn(`[evals] 警告：${label}报告没有进程自证（旧格式），无法确认未被并发覆盖`);
  }
}
if (latest.process?.pid && latest.process.pid === control.process?.pid) {
  refuse('两臂来自同一个进程（pid 相同），不可能是同日两臂各跑一次的独立结果');
}

// 每臂每次运行的原始数字全部打印：红的也要留档，不能被中位数抹掉。
// 本次对照轴（自愈闭环）与既有 A-R6 轴不同：显式说出来，避免读者把两件事混成一件。
if (control.repair_enabled !== latest.repair_enabled) {
  console.log(
    `[evals] 对照轴：自愈闭环 repair_enabled ${fmt(control.repair_enabled)} -> ${fmt(latest.repair_enabled)}`,
  );
}
for (const [label, report, runs] of [
  ['对照(control)', control, cRuns],
  ['实验(treatment)', latest, tRuns],
]) {
  console.log(
    `[evals] ${label} arm=${report.arm} 模型=${report.model} 任务数=${report.summary.n} 运行=${runs.length} 次`,
  );
  for (const run of runs) {
    console.log(
      `  run ${run.run}/${runs.length}: 成功率=${run.summary.passed}/${run.summary.n} ` +
        `轮数=${fmt(run.summary.avg_turns)} token=${fmt(run.summary.avg_total_tokens)}`,
    );
    // 难样本子集（SH-073）：每轮都打，对照组自身的波动范围就靠这几行量出来。
    if (run.summary.hard) {
      const h = run.summary.hard;
      console.log(
        `    难样本: 成功率=${h.passed}/${h.n} 轮数=${fmt(h.avg_turns)} ` +
          `token=${fmt(h.avg_total_tokens)} 自愈触发=${fmt(h.repair_triggered)}`,
      );
    }
    // 事件级（SH-073）：失败事件数、恢复率——自愈的作用对象就是"失败之后的行为"。
    if (run.summary.events) {
      const e = run.summary.events;
      console.log(
        `    事件级: 失败=${fmt(e.failures)} 恢复率=${fmt(e.recovery_rate)} ` +
          `恢复轮次中位数=${fmt(e.median_recovery_turns)}`,
      );
    }
  }
  console.log(
    `  中位数: 成功率=${fmt(median(runs.map((r) => r.summary.success_rate)))} ` +
      `轮数=${fmt(median(runs.map((r) => r.summary.avg_turns)))} ` +
      `token=${fmt(median(runs.map((r) => r.summary.avg_total_tokens)))}`,
  );
}

const med = (runs, key) => median(runs.map((run) => run.summary[key]));
let blocked = false;

const successControl = med(cRuns, 'success_rate');
const successNow = med(tRuns, 'success_rate');
const successDrop = rel(successControl, successNow);
blocked = successDrop < -THRESHOLD;
console.log(
  `  ${'success_rate'.padEnd(17)} ${fmt(successControl)} -> ${fmt(successNow)}  ${pct(successDrop)}  ` +
    `${blocked ? '阻断（硬否决）' : '可接受'}（判据：中位数下降不超 10%）`,
);

for (const [key, label] of [
  ['avg_turns', '平均轮数'],
  ['avg_total_tokens', '平均 token'],
]) {
  const base = med(cRuns, key);
  const now = med(tRuns, key);
  const d = rel(base, now);
  const over = tRuns.filter((run) => rel(base, run.summary[key]) > THRESHOLD).length;
  const beyond = d > THRESHOLD;
  const red = beyond && over === tRuns.length;
  blocked ||= red;
  let verdict = '可接受';
  if (red) verdict = '阻断';
  else if (beyond) verdict = `观察项（${over}/${tRuns.length} 次越阈，不一致不判红）`;
  console.log(
    `  ${key.padEnd(17)} ${fmt(base)} -> ${fmt(now)}  ${pct(d)}  ${verdict}` +
      `（${label}：中位数上升不超 10%，且越阈须每次都一致）`,
  );
}

// 难样本子集（SH-073）：汇总会被简单任务稀释，这一节才是「自愈值不值」的判据。
// 口径硬性：先量对照组自身的波动范围，实验臂必须明显超出才算增益；
// 差异落在波动范围内一律写"无证据"，不许写成"略有提升"。
const hardComplete = [...cRuns, ...tRuns].every((run) => run.summary.hard);
if (!hardComplete) {
  console.log('  [难样本] 报告里没有 hard 子集字段（旧格式），无法单独判读');
} else {
  // infra（零轮结束）是端点抖动的随机噪声，被它影响的一臂会凭空少几分。
  // 因此同一份数据出两组数字：含 infra（保守，记失败）与剔除 infra（敏感性分析）。
  // **两种口径结论不一致时必须说"不可靠"**，不许挑好看的那个当结论。
  const hardView = (run, excludeInfra) => {
    const rows = (run.tasks ?? []).filter((t) => t.hard);
    const infra = rows.filter((t) => t.infra).length;
    const kept = excludeInfra ? rows.filter((t) => !t.infra) : rows;
    const passed = kept.filter((t) => t.success).length;
    return { n: kept.length, passed, infra, rate: kept.length === 0 ? null : passed / kept.length };
  };
  const views = [
    [false, '含 infra（保守）'],
    [true, '剔除 infra（敏感性）'],
  ];
  const verdicts = {};
  for (const [excludeInfra, label] of views) {
    const cV = cRuns.map((run) => hardView(run, excludeInfra));
    const tV = tRuns.map((run) => hardView(run, excludeInfra));
    const cInfra = cV.reduce((sum, v) => sum + v.infra, 0);
    const tInfra = tV.reduce((sum, v) => sum + v.infra, 0);
    console.log(
      `  难样本[${label}] 各轮样本 对照=${cV.map((v) => v.n).join('/')}` +
        ` 实验=${tV.map((v) => v.n).join('/')}（infra 合计 对照=${cInfra} 实验=${tInfra}）`,
    );
    if (cV.some((v) => v.n === 0) || tV.some((v) => v.n === 0)) {
      console.log('    剔除 infra 后某轮样本为 0 → 该口径不可计算');
      verdicts[label] = 'uncomputable';
      continue;
    }
    const cRates = cV.map((v) => v.rate);
    const tRates = tV.map((v) => v.rate);
    const cRange = [Math.min(...cRates), Math.max(...cRates)];
    const tRange = [Math.min(...tRates), Math.max(...tRates)];
    const overlap = tRange[0] <= cRange[1] && cRange[0] <= tRange[1];
    verdicts[label] = overlap ? 'overlap' : 'separated';
    console.log(
      `    对照=${fmt(median(cRates))}（区间 ${fmt(cRange[0])}–${fmt(cRange[1])}）` +
        ` -> 实验=${fmt(median(tRates))}（区间 ${fmt(tRange[0])}–${fmt(tRange[1])}）`,
    );
    console.log(
      overlap
        ? '    区间重叠 → 无证据（差异落在对照自身波动范围内，不得写成提升）'
        : '    区间不重叠 → 需人工确认差异是否由本次变更引入，再决定是否够格算增益',
    );
  }
  const verdictList = views.map(([, label]) => verdicts[label]);
  if (verdictList[0] !== verdictList[1]) {
    console.log(
      '    ⚠ 两种口径结论不一致 → 结论受 infra 分布影响，不可靠；不得挑好看的那个当结论。',
    );
  } else {
    console.log('    两种口径结论一致 → 结论对 infra 分布稳健。');
  }
  const hit = tRuns.map((run) => run.summary.hard.repair_triggered);
  console.log(
    `    实验臂每轮自愈触发次数: ${hit.join(' / ')}` +
      `（全 0 说明难样本根本没走到自愈路径，结论应是"不适用"而不是"无效"）`,
  );
  // 可探测性：二元结果 + 每臂 n 个样本时，本实验能看见的最小差异。
  // 推算（两比例检验，α=0.05 双侧、功效 80% → z_α/2+z_β ≈ 2.8；
  // 基线成功率取 0.5，方差最大即最坏情形）：MDE = 2.8 * sqrt(2*0.5*0.5/n)。
  // 必须打印：否则"未观察到差异"会被读成"无效"，那是把测不出说成不存在。
  const perArm = tRuns.length * (tRuns[0].summary.hard.n ?? 0);
  const mde = 2.8 * Math.sqrt(0.5 / Math.max(perArm, 1));
  console.log(
    `    可探测性: 每臂 ${perArm} 个二元样本 → 最小可探测差异 ≈ ${(mde * 100).toFixed(0)}pp` +
      `（α=0.05 双侧、功效 80%、基线 p=0.5 最坏情形）`,
  );
  if (mde > 0.2) {
    const need = Math.ceil(0.5 / Math.pow(0.2 / 2.8, 2));
    console.log(
      `    要探测 20pp 级差异需每臂约 ${need} 个样本（当前 ${perArm}）——` +
        `"未观察到差异"不等于"无效"，也不等于"有效"`,
    );
  }
}

// 事件级对照（SH-073）：只回答"失败之后机制有没有工作"。
// 硬规则：两臂失败事件数中位数相差 1.5 倍以上 → 事件级对比作废，只报任务级。
// 事件级数字**不得**被用来宣称任务成功率提升，那是任务级的事。
const hasEvents = [...cRuns, ...tRuns].every((run) => run.summary.events);
if (!hasEvents) {
  console.log('  [事件级] 报告里没有 events 字段（旧格式），无法单独判读');
} else {
  const cFail = median(cRuns.map((r) => r.summary.events.failures));
  const tFail = median(tRuns.map((r) => r.summary.events.failures));
  const ratio = Math.max(cFail, tFail) / Math.max(Math.min(cFail, tFail), 1);
  console.log(`  事件级 失败事件中位数: 对照=${cFail} 实验=${tFail}（比值 ${ratio.toFixed(2)}）`);
  if (ratio >= 1.5) {
    console.log('    失败事件数不相当（>= 1.5 倍）→ 事件级对比作废，只报任务级');
  } else {
    const cRate = median(cRuns.map((r) => r.summary.events.recovery_rate));
    const tRate = median(tRuns.map((r) => r.summary.events.recovery_rate));
    const cLat = median(cRuns.map((r) => r.summary.events.median_recovery_turns));
    const tLat = median(tRuns.map((r) => r.summary.events.median_recovery_turns));
    console.log(
      `    恢复率: 对照=${fmt(cRate)} -> 实验=${fmt(tRate)}；` +
        `恢复轮次中位数: 对照=${fmt(cLat)} -> 实验=${fmt(tLat)}（窗口 N=3 轮）`,
    );
    console.log(
      '    口径：事件级只说明"自愈触发后有没有起作用"；任务交付好不好看上面的任务级',
    );
  }
}

// 冻结基线：只作历史参考。漂移幅度说明它为什么不能当分母。
const history = load(`baseline.${latest.model}.json`, { required: false });
if (history) {
  const drift = rel(history.summary.avg_total_tokens, med(cRuns, 'avg_total_tokens'));
  console.log(
    `  [参考] 冻结基线（${dateOf(history.epoch_ms)}，单次样本）token=${fmt(history.summary.avg_total_tokens)}，` +
      `与今日对照相差 ${pct(drift)}；仅作历史记录，**不参与判定**。`,
  );
}

if (latest.summary.passed === 0) {
  console.error('[evals] 全数失败：按运行器口径这是环境故障（端点/沙箱/审批），不得当作能力数据合并。');
  blocked = true;
}

console.log(blocked ? '[evals] 结论：退化超阈，阻断合并。' : '[evals] 结论：基线回归通过。');
process.exit(blocked ? 1 : 0);
