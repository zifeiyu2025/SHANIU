# N2–N9 真机复验任务

> 规格：[spec.md](file:///d:/MyWorkspace/SHANIU/docs/specs/N2-N9-live-reverify/spec.md)
> 判据来源：`docs/smoke/2026-11-17-9af8431/REPORT.md` 第 60–67 行（不改）
> 被测 exe：`dist-portable\SHANIU-Portable\SHANIU.exe`（SHA256 `0ABD6B0A…DEC071`）

## T1 场景化模型端点

- [x] 新建 `scripts/smoke/scenario-model.mjs`
  - `startScenarioStub({ port, scenario, requestLog })`
  - 辅助请求分类：`title` / `compaction` 不推进步号；`main` 推进
  - SSE 支持文本步、多 `tool_calls` 分片、`finish_reason` = `stop|length|tool_calls`
  - 步数用尽回固定文本并记 `overflow: true`
  - 每个主请求写一行 `requests.jsonl`
  - 追加 `usage: 'none'`：模拟不回报用量的端点（N7 只裁剪不压缩的唯一入口）
- 验收：本地起服务跑文本步、两调用步、不回报用量三形态，SSE 逐行 `JSON.parse` 通过。
- 不改 `scripts/smoke/stub-model.mjs`。

## T2 真机驱动骨架

- [x] 新建 `scripts/smoke/n2n9-live.mjs`
  - 参数 `--scenario --exe --out`
  - 生成 `<out>/<scenario>/data`（配置模板见 spec §3.2）与 `<out>/<scenario>/workspace`
  - `launch/kill/relaunch`、`storeKey`、`openPriorSession`、`refreshSession`、`footer`、
    `cardState/collectCards/expandCard`、`expandProcessGroups`、`exportZip`、`rolloutPath`、
    `turnEnds`、`bodyText/fullText`、`shot`、`verdict.json`
  - 原生「另存为」：保存对话框脚本落 `<scenario>-save-dialog.ps1`，执行日志同目录
- 验收：`--scenario n3` 能启动、发消息、拿到回复、退出码 0/1 可区分。

## T3 N3 截断

- [x] 单步 `finish_reason:length`，实时与重开断言四条（spec §4 N3）
- 证据：`n3-live.png`、`n3-reopen.png`、`n3-requests.jsonl`、`n3-rollout.jsonl`、`n3-verdict.json`

## T4 N2 审批挂起强杀

- [x] 审批卡出现后强杀，重开断言五条（`indeterminate` / 未知 / 无卡 / 失效通知 / 不出现已停止）
- 取证要点：失效通知落在默认折叠的回合过程组内，必须 `expandProcessGroups()` 后再判
- 证据：`n2-before-kill.png`、`n2-reopen.png`、`n2-requests.jsonl`、`n2-rollout.jsonl`、`n2-verdict.json`

## T5 N5 挂起强杀步数

- [x] 前置「2 轮 3 步」（第 2 轮 read + 收尾两步），第 3 轮挂起强杀；重开页脚与强杀前逐项相等
- [x] 挂起回合落一条 `turn_end reason=indeterminate` 且不带 `iterations`
- 证据：`n5-before-kill.png`、`n5-reopen.png`、`n5-rollout-{before,after}.jsonl`、`n5-verdict.json`

## T6 N6 拒绝/越界折叠态

- [x] 三次调用：越界 read、marker 负例 read、被拒 exec
- [x] 实时与重开都断言 `data-state`：`c-n6-out`/`c-n6-deny` 非 `ok`，`c-n6-marker` 为 `ok`
- [x] 实时只判「失败」标记（实时流只有错误码），完整拒绝说明按重开判
- 证据：`n6-live.png`、`n6-reopen.png`、`n6-cards-{live,reopen}.json`、`n6-verdict.json`

## T7 N7 裁剪留痕

- [x] `token_budget=5000` + 端点不回报用量 → 4 万字符级工具输出触发 D6 裁剪（38958 字符）
- [x] 实时通知、重开判读行、rollout 原文完整、导出扫「已裁剪」> 0
- 证据：`n7-live.png`、`n7-live-expanded.png`、`n7-reopen.png`、`n7-session.v4.jsonl`、`n7-rollout.jsonl`、`n7-verdict.json`

## T8 N4 导出步数

- [x] 2 回合 × 2 步，导出 ZIP 取 `session.v4.jsonl`
- [x] `turn/end.iterations` 与 rollout 一致；`step` 非恒 1（0 起，`0/1`）；`0 <= step < iterations`
- [x] 旧档子例：删 `turn_end.iterations` 后导出 → `stepsUnavailable: true` 且不带 `step`
- 证据：`n4-session.v4.jsonl`、`n4-legacy-session.v4.jsonl`、`n4-rollout.jsonl`、`n4-export.png`、`n4-verdict.json`

## T9 N8 多块补丁失败回执

- [x] 同一 `*** Update File` 段内两块，第 2 块锚在预演结果上必失败
  （两次 `Update File` 头会按段顺序写盘，验不到同一 `apply_hunks` 口径）
- [x] 回执「实际读到 `a1`」，不含 `a1-first`；`data.txt` 未被改动
- 证据：`n8-live.png`、`n8-reopen.png`、`n8-card.json`、`n8-card-reopen.json`、`n8-reopen-text.txt`、`n8-data-after.txt`、`n8-verdict.json`

## T10 N9 读取记录过期文案

- [x] 第 1 回合 read+patch，第 2 回合直接 patch
- [x] 错误卡正文含新文案、不含「未读取过」；文件保持第 1 回合结果
- 证据：`n9-live.png`、`n9-reopen.png`、`n9-card-reopen.json`、`n9-note-after.txt`、`n9-verdict.json`

## T11 落档与报告

- [x] 八份 `verdict.json` 汇总（72 条判据 0 失败）
- [x] `docs/smoke/2026-11-17-9af8431/REPORT.md` 追加「后续复验」节（原表不动）
- [x] `docs/evidence/2026-11-17/README.md` 追加索引节
- [x] `git status` 只含：2 个新脚本、证据、REPORT/README、spec
- [x] 向用户报告逐项结论；提交与推送等用户确认

## T12 全量门禁（仅当改动触到产品代码时）

- [x] 本轮未改产品代码（`src/`、`src-tauri/` 均无改动），无需补测。
