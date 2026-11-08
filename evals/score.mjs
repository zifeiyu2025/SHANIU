#!/usr/bin/env node
// 评估基线回归门禁（docs/05 SH-028、docs/04 §6.5、AGENTS.md §5）。
//
// 只读两份报告做判定，不联网、不起 Agent：跑由
//   cargo test --test evals_baseline -- --ignored --nocapture
// 负责（默认 5 次，SHANIU_EVAL_REPEATS 可覆盖），本脚本负责"这次变更能不能合并"这个结论。
//
// 用法：
//   node evals/score.mjs                 # latest 对 baseline 判退化（CI/合并前）
//   node evals/score.mjs --promote       # 认可 latest，升格为新 baseline（有意重定基线时）
//
// 判据（相对变化，阈值 10% 不变，**只改怎么得出结论**）：
//   - 样本：latest 必须带 >= 5 次运行；不足 5 次直接 exit 2（不下结论），不拿单次运行判红绿。
//   - success_rate：中位数下降 > 10% → 硬否决（任务能力退化）。
//   - avg_turns / avg_total_tokens：中位数上升 > 10% 且**每一次运行都越阈**才判红；
//     只有中位数越阈、运行间不一致 → 记为观察项，不阻断。
//     理由：端点漂移曾让基线整体上抬 32%/轮，单次或忽高忽低的偏移不是能力信号。
//   - 基线数值有时效性：跨日对比无效，必须同日、同端点做 A/B。
//
// 无第三方依赖（node:fs 足够）：评分脚本必须能在任何 runner 上直接跑，
// 引入 npm 依赖等于给交付管线再加一条供应链。

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const THRESHOLD = 0.1;
const MIN_RUNS = 5;
const promote = process.argv.includes('--promote');

function load(name) {
  try {
    return JSON.parse(readFileSync(join(HERE, name), 'utf8'));
  } catch (e) {
    console.error(`[evals] 读不到 ${name}：${e.message}`);
    console.error('[evals] 先跑：SHANIU_TEST_API_KEY=... cargo test --test evals_baseline -- --ignored --nocapture');
    process.exit(2);
  }
}

const latest = load('latest.json');
// 基线按**模型**分档存（evals/baseline.<model>.json）：不同模型的轮数/token 量级不可比，
// 混在一份文件里会让"换模型"这种正常变更每次都撞退化门禁，久而久之门禁就没人信了。
const baseName = `baseline.${latest.model}.json`;
if (promote) {
  writeFileSync(join(HERE, baseName), `${JSON.stringify(latest, null, 2)}\n`);
  console.log(
    `[evals] 已将 latest 升格为 ${baseName}（成功率中位数=${latest.summary.success_rate}，样本=${latest.repeats ?? 1} 次）`,
  );
  process.exit(0);
}

const base = load(baseName);

const fmt = (v) => (typeof v === 'number' ? (Number.isInteger(v) ? `${v}` : v.toFixed(3)) : `${v}`);
const pct = (d) => `${d >= 0 ? '+' : ''}${(d * 100).toFixed(1)}%`;

function rel(a, b) {
  if (a === 0) return b === 0 ? 0 : Number.POSITIVE_INFINITY;
  return (b - a) / a;
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[mid - 1] + sorted[mid]) / 2 : sorted[mid];
}

if (base.base_url !== latest.base_url) {
  console.error(
    `[evals] 不可比：${baseName} 端点=${base.base_url}，本轮=${latest.base_url}\n` +
      '      同模型换端点不能当回归证据；确属新配置就 --promote 重建该档基线。',
  );
  process.exit(1);
}

const runs = Array.isArray(latest.runs) ? latest.runs : [];
if (runs.length < MIN_RUNS) {
  console.error(
    `[evals] 样本不足：本轮只有 ${runs.length} 次运行，门禁要求 >= ${MIN_RUNS} 次。\n` +
      '      单次运行会被端点漂移支配（实测能造成 32%/轮的 token 偏移），不能据此判红绿。\n' +
      '      重跑：SHANIU_EVAL_REPEATS=5 cargo test --test evals_baseline -- --ignored --nocapture',
  );
  process.exit(2);
}

// 每次运行的原始数字全部打印：红的也要留档，不能在结论里被"中位数"抹掉。
console.log(`[evals] 模型=${latest.model} 任务数=${latest.summary.n} 运行次数=${runs.length}（取中位数判定）`);
for (const run of runs) {
  console.log(
    `  run ${run.run}/${runs.length}: 成功率=${run.summary.passed}/${run.summary.n} ` +
      `轮数=${fmt(run.summary.avg_turns)} token=${fmt(run.summary.avg_total_tokens)}`,
  );
}

const baseRuns = Array.isArray(base.runs) ? base.runs : [];
if (baseRuns.length < MIN_RUNS) {
  console.log(
    `[evals] 注意：基线 ${baseName} 只有 ${baseRuns.length || 1} 次样本，` +
      '本轮参照仍是它的 summary；下一轮请 --promote 用 5 次样本重建基线。',
  );
}

const med = (key) => median(runs.map((run) => run.summary[key]));
const baseValue = (key) => base.summary[key];

// success_rate：下降即能力退化，中位数一越阈就硬否决。
const successBase = baseValue('success_rate');
const successNow = med('success_rate');
const successDrop = rel(successBase, successNow);
let blocked = successDrop < -THRESHOLD;
console.log(
  `  ${'success_rate'.padEnd(17)} ${fmt(successBase)} -> ${fmt(successNow)}  ${pct(successDrop)}  ` +
    `${blocked ? '阻断（硬否决）' : '可接受'}（判据：中位数下降不超 10%）`,
);

// 轮数 / token：中位数越阈且每次运行都越阈才算退化；否则只是观察项。
for (const [key, badDir, label] of [
  ['avg_turns', 'up', '平均轮数'],
  ['avg_total_tokens', 'up', '平均 token'],
]) {
  const b = baseValue(key);
  const now = med(key);
  const d = rel(b, now);
  const perRun = runs.map((run) => rel(b, run.summary[key]));
  const over = perRun.filter((one) => (badDir === 'down' ? one < -THRESHOLD : one > THRESHOLD)).length;
  const consistent = over === runs.length;
  const beyond = badDir === 'down' ? d < -THRESHOLD : d > THRESHOLD;
  const red = beyond && consistent;
  blocked ||= red;
  let verdict = '可接受';
  if (red) verdict = '阻断';
  else if (beyond) verdict = `观察项（${over}/${runs.length} 次越阈，不一致不判红）`;
  console.log(
    `  ${key.padEnd(17)} ${fmt(b)} -> ${fmt(now)}  ${pct(d)}  ${verdict}` +
      `（${label}：中位数上升不超 10%，且越阈须每次都一致）`,
  );
}

const taskSource = Array.isArray(latest.tasks) ? latest.tasks : runs[0].tasks;
const byId = new Map(base.tasks.map((t) => [t.id, t]));
for (const t of taskSource) {
  const prev = byId.get(t.id);
  if (prev && prev.success !== t.success) {
    console.log(`  · ${t.id}: ${prev.success ? 'PASS' : 'FAIL'} -> ${t.success ? 'PASS' : 'FAIL'}  ${t.detail}`);
  }
}
if (latest.summary.passed === 0) {
  console.error('[evals] 全数失败：按运行器口径这是环境故障（端点/沙箱/审批），不得当作能力数据合并。');
  blocked = true;
}

console.log(blocked ? '[evals] 结论：退化超阈，阻断合并。' : '[evals] 结论：基线回归通过。');
process.exit(blocked ? 1 : 0);
