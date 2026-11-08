#!/usr/bin/env node
// 评估基线回归门禁（docs/05 SH-028、docs/04 §6.5、AGENTS.md §5）。
//
// 只读三份报告做判定，不联网、不起 Agent。跑由
//   cargo test --test evals_baseline -- --ignored --nocapture
// 负责（默认 5 次，SHANIU_EVAL_REPEATS 可覆盖），本脚本负责"这次变更能不能合并"。
//
// **分母是当日对照，不是冻结基线**：端点漂移实测把整份基线抬升 32%/轮，
// 冻结点已经不是同一把尺子；拿它当分母，结论永远红且红得没有意义。
//   对照臂：SHANIU_EVAL_ROLE=control SHANIU_EVAL_ABLATE_AR6=1  → evals/control.json
//   实验臂：SHANIU_EVAL_ROLE=treatment（默认，条款常驻）      → evals/latest.json
// 两臂必须同日、同端点、同模型、同参数（任务集/轮数上限/重复次数），否则拒绝下结论。
//
// 用法：
//   node evals/score.mjs                 # 当日对照 vs 当日实验，判退化
//   node evals/score.mjs --promote       # 把实验臂写成该模型的历史参考点（仅参考，不参与判定）
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

const latest = load('latest.json');
if (promote) {
  const name = `baseline.${latest.model}.json`;
  writeFileSync(join(HERE, name), `${JSON.stringify(latest, null, 2)}\n`);
  console.log(`[evals] 已把 latest 写为历史参考点 ${name}（不参与判定）`);
  process.exit(0);
}

const control = load('control.json');
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
if (cRuns.length < MIN_RUNS || tRuns.length < MIN_RUNS) {
  refuse(
    `样本不足（对照 ${cRuns.length} 次 / 实验 ${tRuns.length} 次），两臂各需 >= ${MIN_RUNS} 次。\n` +
      '      重跑：SHANIU_EVAL_REPEATS=5 ...',
  );
}
if (control.arm === latest.arm) {
  refuse(`两臂 arm 相同（${latest.arm}），对照臂必须带 SHANIU_EVAL_ABLATE_AR6=1`);
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

// 每臂每次运行的原始数字全部打印：红的也要留档，不能被中位数抹掉。
for (const [label, report, runs] of [
  ['对照(A-R6 关)', control, cRuns],
  ['实验(A-R6 开)', latest, tRuns],
]) {
  console.log(
    `[evals] ${label} arm=${report.arm} 模型=${report.model} 任务数=${report.summary.n} 运行=${runs.length} 次`,
  );
  for (const run of runs) {
    console.log(
      `  run ${run.run}/${runs.length}: 成功率=${run.summary.passed}/${run.summary.n} ` +
        `轮数=${fmt(run.summary.avg_turns)} token=${fmt(run.summary.avg_total_tokens)}`,
    );
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
