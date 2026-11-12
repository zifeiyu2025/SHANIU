#!/usr/bin/env node
// 真实仓库端到端评测（单臂）报告阅读器。
//
// 只读一份报告，不联网、不起 Agent：
//   node evals/score-repo.mjs evals/results/<epoch>-<pid>-repo.json
//
// 判定口径：
//   - `valid:false` 的报告**显著标红并以退出码 1 结束**：invalid 题不计分母这类事
//     必须由读取方看见，不能藏在一行 JSON 字段里；
//   - 正常报告打印每题一行（id/type/✓✗/轮次/打断/耗时/invalid 原因）与汇总统计；
//   - 分母 = 有效题（invalid 已剔除），超时另算超时率，轮次未知（超时）不计入均值。

import { readFileSync } from 'node:fs';

const RED = '\x1b[31m';
const BOLD = '\x1b[1m';
const DIM = '\x1b[2m';
const RESET = '\x1b[0m';

const file = process.argv[2];
if (!file) {
  console.error('用法：node evals/score-repo.mjs <report.json>');
  process.exit(2);
}

let report;
try {
  report = JSON.parse(readFileSync(file, 'utf8'));
} catch (error) {
  console.error(`${RED}${BOLD}[repo-eval] 读不到报告 ${file}：${error.message}${RESET}`);
  process.exit(2);
}

const fmt = (value) =>
  typeof value === 'number' ? (Number.isInteger(value) ? `${value}` : value.toFixed(2)) : 'n/a';
const pct = (value) => (typeof value === 'number' ? `${(value * 100).toFixed(1)}%` : 'n/a');
const ms = (value) => (typeof value === 'number' ? `${Math.round(value)}ms` : 'n/a');
const pair = (values) =>
  Array.isArray(values) ? `[${values.map(pct).join(', ')}]` : 'n/a';

const tasks = Array.isArray(report.tasks) ? report.tasks : [];
const summary = report.summary ?? {};
const validity = report.validity ?? {};
const repo = report.repo ?? {};
const durations = summary.duration ?? {};
const interruptions = summary.interruptions ?? {};

console.log(
  `${BOLD}[repo-eval]${RESET} base_commit=${report.base_commit ?? '?'} ` +
    `arm=${report.arm ?? '?'} model=${report.model ?? '?'} base_url=${report.base_url ?? '?'}`,
);
console.log(
  `           tasks=${report.tasks_file ?? '?'} repeats=${fmt(report.repeats)} mode=${report.mode ?? '?'} ` +
    `pid=${report.process?.pid ?? '?'} 快照文件=${fmt(repo.snapshot_files)} 预热=${ms(repo.warmup_ms_total)}`,
);

const valid = report.valid === true;
const invalidFlags = Object.entries(validity)
  .filter(([, value]) => value !== true)
  .map(([key, value]) => `${key}=${value}`);
if (!valid) {
  console.log(
    `${RED}${BOLD}!! 报告无效（valid=false）——禁止据此下结论${RESET}` +
      `${invalidFlags.length ? `${RED}：${invalidFlags.join(' ')}${RESET}` : ''}`,
  );
}

// 每题一行：invalid 用 `—`（它不进分母），失败用 `✗`。
for (const task of tasks) {
  const verdict = task.invalid ? '—' : task.success ? '✓' : '✗';
  const turns = task.turns === null || task.turns === undefined ? 'unknown' : `${task.turns}`;
  let line =
    `  ${verdict} ${String(task.id).padEnd(34)} ${String(task.type ?? '?').padEnd(9)} ` +
    `turns=${turns.padEnd(7)} 打断=${fmt(task.interruptions)} 耗时=${ms(task.agent_ms)} ` +
    `files=${fmt(task.files_changed)}`;
  if (task.timeout) line += ' 超时';
  if (task.forbidden_touched) line += ' 命中禁止路径';
  // 逐题证据三项：base_commit / 补丁校验 / 前置红。缺一项按 invalid 明示（不计分母）。
  const missing = [];
  if (!task.base_commit) missing.push('base_commit');
  if (!task.patch_check) missing.push('patch_check');
  if (task.precondition_red !== true && !task.invalid) missing.push('precondition_red');
  if (missing.length) {
    line += ` 证据缺失=${missing.join(',')}`;
  }
  if (task.patch_check && task.patch_check !== 'ok' && task.patch_check !== 'none') {
    line += ` 补丁校验=${task.patch_check}`;
  }
  if (task.warmup_ok === false) line += ' 预热未编译';
  if (task.invalid) {
    line += ` invalid：${task.invalid_reason ?? '（未说明）'}`;
    console.log(`${RED}${line}${RESET}`);
  } else {
    console.log(line);
    if (!task.success) console.log(`${DIM}      ${task.detail ?? ''}${RESET}`);
  }
}

// 每一轮的原始数字都打印：中位数不负责藏起波动。
const runs = Array.isArray(report.runs) ? report.runs : [];
if (runs.length > 1) {
  console.log(`${BOLD}每轮${RESET}`);
  for (const run of runs) {
    const s = run.summary ?? {};
    console.log(
      `  run ${run.run}/${runs.length}: 成功率=${fmt(s.passed)}/${fmt(s.n)} ${pct(s.success_rate)} ` +
        `轮次 均值/中位/P90=${fmt(s.avg_turns)}/${fmt(s.median_turns)}/${fmt(s.p90_turns)} ` +
        `超时率=${pct(s.timeout_rate)} 打断=${fmt(s.interruptions?.total)}`,
    );
  }
}

console.log(`${BOLD}汇总（${fmt(report.repeats)} 轮中位数）${RESET}`);
console.log(
  `  完成率 ${fmt(summary.passed)}/${fmt(summary.n)} ${pct(summary.success_rate)} ` +
    `Wilson95=${pair(summary.success_rate_ci95)}（invalid=${fmt(summary.invalid)}，单列不计分母）`,
);
console.log(
  `  轮次 均值/中位/P90=${fmt(summary.avg_turns)}/${fmt(summary.median_turns)}/${fmt(summary.p90_turns)} ` +
    `未知=${fmt(summary.turns_unknown)} 上限=${fmt(summary.max_turns)}`,
);
console.log(
  `  打断 合计=${fmt(interruptions.total)} 均值=${fmt(interruptions.mean)} ` +
    `中位=${fmt(interruptions.median)} P90=${fmt(interruptions.p90)} ` +
    `有打断的题=${fmt(interruptions.tasks_with_any)}`,
);
console.log(
  `  超时率=${pct(summary.timeout_rate)} 耗时 P50/P90=${ms(durations.agent_ms_p50)}/${ms(
    durations.agent_ms_p90,
  )} agent合计=${ms(durations.agent_ms_total)}`,
);
console.log(
  `  样本校验 档案=${fmt(report.samples)}；无超时口径成功率=${pct(summary.success_rate_no_timeout)}`,
);

process.exit(valid ? 0 : 1);
