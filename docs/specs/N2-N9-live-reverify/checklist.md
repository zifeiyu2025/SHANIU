# N2–N9 真机复验检查单

> 依据 [spec.md](file:///d:/MyWorkspace/SHANIU/docs/specs/N2-N9-live-reverify/spec.md) §7 验收标准与 §9 执行期校正。

## 交付完整性

- [x] 八个场景各有 `<n>-verdict.json`，且 `checks` 全部 `ok: true`（实测 8/9/13/7/15/10/5/5 共 72 条，0 失败）
- [x] 每个场景至少一张截图，证据目录共 19 张 PNG，覆盖实时与重开两态
- [x] 被测 exe 与 N1 同一份（`n2-verdict.json` 记录 SHA256 `0ABD6B0A…DEC071`）
- [x] 每场景独立数据根与工作区，工作区根在数据根之外

## 判据落档一致性

- [x] `REPORT.md` 追加「N2–N9 真机复验」节，8 行结论与各 `verdict.json` 逐条一致（全 PASS）
- [x] `REPORT.md` 原十项总表、六项复验、N1–N9 原结论零改动（diff 仅 +37/−1，删除行是上节「尚未重跑」提示）
- [x] `docs/evidence/2026-11-17/README.md` 追加同名索引节，纯追加（+49/−0）
- [x] 三条附带观察（实时/重开文案不一致、两级折叠影响取证、裁剪与压缩触发条件）在 REPORT 与 README 同口径记录

## 零侵入

- [x] `git diff --stat -- src src-tauri` 为空，产品代码未改
- [x] `git status --short` 仅：`scripts/smoke/n2n9-live.mjs`、`scripts/smoke/scenario-model.mjs`、`docs/evidence/2026-11-17/n2n9-live/`、`docs/evidence/2026-11-17/README.md`、`docs/smoke/2026-11-17-9af8431/REPORT.md`、`docs/specs/N2-N9-live-reverify/`
- [x] 未改 `scripts/smoke/stub-model.mjs`（`run-smoke.mjs` 依赖其固定回复）
- [x] 工作区外临时探针已清理（`%TEMP%` 下 `n2n9-*`、`n2probe*` 均无残留）

## 关键判据抽查

- [x] N2：档案 `turn_end.reason=indeterminate`，重开无审批卡、无「已停止」、有「已失效」通知，步数不变
- [x] N3：实时与重开各恰好 1 条截断提示，组标题「已截断」，无「本轮运行失败」「已完成」
- [x] N4：导出 `turn/end.iterations=[2,2]`；`step` 为 `0/1` 且 `0 <= step < iterations`；旧档 `stepsUnavailable:true` 且不带 `step`
- [x] N5：强杀前后页脚一致（2 轮 3 步），挂起回合 `indeterminate` 且不带 `iterations`
- [x] N6：`c-n6-out`/`c-n6-deny` 非 `ok` 且有「失败」标记，`c-n6-marker` 仍 `ok`，重开保留完整拒绝说明
- [x] N7：`38958` 字符触发裁剪（头 4096 + 尾 1024），实时有通知，重开卡有 `[已裁剪]` 判读行，档案保留原文，导出「已裁剪」1 处
- [x] N8：回执「实际读到 `a1`」且不含 `a1-first`，`data.txt` 整单回滚
- [x] N9：文案为「在本任务内没有可用的读取记录（…内容可能已过期）」，无「未读取过」，`note.txt` 保持 `alpha-1`

## 待用户决定

- [x] 已报告逐项结论；提交与推送未执行，等用户确认
