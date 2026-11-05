#!/usr/bin/env node
// 评估基线回归门禁（docs/05 SH-028、docs/04 §6.5、AGENTS.md §5）。
//
// 只读两份报告做判定，不联网、不起 Agent：跑由
//   cargo test --test evals_baseline -- --ignored --nocapture
// 负责，本脚本负责"这次变更能不能合并"这个二值结论。
//
// 用法：
//   node evals/score.mjs                 # latest 对 baseline 判退化（CI/合并前）
//   node evals/score.mjs --promote       # 认可 latest，升格为新 baseline（有意重定基线时）
//
// 判据（相对变化，阈值 10%）：
//   success_rate    下降 > 10%  → 阻断（任务能力退化）
//   avg_turns       上升 > 10%  → 阻断（同样任务要多绕轮次）
//   avg_total_tokens 上升 > 10% → 阻断（上下文膨胀，直接对应成本）
// 另两条硬规则：模型/端点变了不可比 → 阻断；latest 全 0 分 → 判环境故障，阻断。
//
// 无第三方依赖（node:fs 足够）：评分脚本必须能在任何 runner 上直接跑，
// 引入 npm 依赖等于给交付管线再加一条供应链。

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const THRESHOLD = 0.1;
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
if (promote) {
  writeFileSync(join(HERE, 'baseline.json'), `${JSON.stringify(latest, null, 2)}\n`);
  console.log(`[evals] 已将 latest 升格为 baseline（模型=${latest.model} 成功率=${latest.summary.success_rate}）`);
  process.exit(0);
}

const base = load('baseline.json');

const fmt = (v) => (typeof v === 'number' ? (Number.isInteger(v) ? `${v}` : v.toFixed(3)) : `${v}`);
const pct = (d) => `${d >= 0 ? '+' : ''}${(d * 100).toFixed(1)}%`;

function rel(a, b) {
  if (a === 0) return b === 0 ? 0 : Number.POSITIVE_INFINITY;
  return (b - a) / a;
}

if (base.model !== latest.model || base.base_url !== latest.base_url) {
  console.error(
    `[evals] 不可比：baseline=${base.model}@${base.base_url} latest=${latest.model}@${latest.base_url}\n` +
      '      换端点/换模型的数字不能当回归证据；要新基线就用 --promote 显式定基线。',
  );
  process.exit(1);
}

const rows = [
  ['success_rate', base.summary.success_rate, latest.summary.success_rate, 'down'],
  ['avg_turns', base.summary.avg_turns, latest.summary.avg_turns, 'up'],
  ['avg_total_tokens', base.summary.avg_total_tokens, latest.summary.avg_total_tokens, 'up'],
];

console.log(`[evals] 模型=${latest.model} 任务数=${latest.summary.n}`);
let blocked = false;
for (const [key, b, l, badDir] of rows) {
  const d = rel(b, l);
  const worse = badDir === 'down' ? d < -THRESHOLD : d > THRESHOLD;
  blocked ||= worse;
  console.log(
    `  ${key.padEnd(17)} ${fmt(b)} -> ${fmt(l)}  ${pct(d)}  ${worse ? '阻断' : '可接受'}（判据：${badDir === 'down' ? '下降不超 10%' : '上升不超 10%'}）`,
  );
}

const byId = new Map(base.tasks.map((t) => [t.id, t]));
for (const t of latest.tasks) {
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
