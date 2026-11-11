# 上游对齐账本（机器生成）

> 由 `scripts/align-report.mjs` 生成，请勿手改；人工说明维护在 `align/overrides.json`。
> `status` 四值：`same` / `drift` / `missing` / `not-applicable`（逐文件 sha256 比对）。
> `review` 两值：`已登记` / `needs-review`（后者=还没有人工记录，必须补 overrides）。

## 统计

- 条目总数：3314
- same：2680
- drift：75
- missing：0
- not-applicable：559
- 待复核（needs-review）：0

## 不一致条目

| 文件 | 状态 | 人工记录 | 说明 | 兜底 |
| --- | --- | --- | --- | --- |
| `packages/client/AGENTS.md` | not-applicable | 已登记 | 上游 packages/client 顶层文档，不随 vendor 引入 | 无需同步 |
| `packages/client/README.i18n.yaml` | not-applicable | 已登记 | 上游 packages/client 顶层文档，不随 vendor 引入 | 无需同步 |
| `packages/client/README.md` | not-applicable | 已登记 | 上游 packages/client 顶层文档，不随 vendor 引入 | 无需同步 |
| `packages/client/README.zh.md` | not-applicable | 已登记 | 上游 packages/client 顶层文档，不随 vendor 引入 | 无需同步 |
| `packages/client/file-upload/src/client/runtime.ts` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复，不改运行行为（docs/07 #9） | 上游升级 React 19 类型后重跑 tsc -b；上游已修则删本地改动 |
| `packages/client/hmr/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/src/events.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/tests/events.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/tests/node-half.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/tests/transport.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/hmr/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/tsdown.client.ts` | not-applicable | 已登记 | 上游 client 包群构建入口；本项目走 Vite 自有构建链 | 无需同步 |
| `packages/client/ui-agent-preset/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetLabel.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetLabel.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetSeat.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetSeat.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetSection.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/AgentPresetSection.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/CreatePluginMenuItem.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/CreatePluginMenuItem.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/PresetGuideDialog.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/PresetGuideDialog.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/analytics.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/guide-locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/seat-store.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/section-store.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/client/settings-store.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/analytics.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/components.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/create-plugin-menu-item.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/locales.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/section-store.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/section.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tests/settings-store.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-preset/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamBoard.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamBoard.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamCommandRow.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamCommandRow.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamDisableConfirmation.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamDisableConfirmation.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamModeToggle.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/TeamModeToggle.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/client/mount.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/browser-plugin.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/team-board.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/team-command.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/team-disable-confirmation-callbacks.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/team-disable-confirmation.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tests/team-mode.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-agent-team/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-approval/src/client/ApprovalCard.tsx` | drift | 已登记 | 默认焦点落「拒绝」；修 allow-once 被记忆为永久放行（docs/07 #6） | 同步时确认上游未把一次性放行降级为 allow_always（§11.3 退化红线） |
| `packages/client/ui-approval/src/client/ApprovalPanel.tsx` | drift | 已登记 | A0-T7 审批详情结构化：description 可选透传（docs/07 #25） | 上游若把审批详情做成结构化组件，删本地实现改跟上游；必须确认 description 仍是 optional |
| `packages/client/ui-approval/src/client/contract/slots.ts` | drift | 已登记 | A0-T7：审批详情槽所在契约，description 可选（docs/07 #25） | 同步上游后确认详情槽仍是单定义、字段仍可选 |
| `packages/client/ui-approval/src/client/index.ts` | drift | 已登记 | A0-T7：面板导出与槽接线（docs/07 #25） | 上游若提供等价入口则改调上游 |
| `packages/client/ui-brand-official/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/Brand.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/assets/breach-atlas.webp` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/home-whale/PixelWhale.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/home-whale/PixelWhale.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/home-whale/artwork.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/home-whale/code-fill.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/home-whale/renderer.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/tests/browser-plugin.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/tests/home-whale.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/tests/pixel-whale.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-brand-official/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `src/dsh/packages/client/ui-chat/src/client/chat/ApprovalCommand.module.css` | not-applicable | 已登记 | A0-T7 本地新增样式（上游无此文件，docs/07 #25） | 上游若自带详情样式，删本地文件 |
| `packages/client/ui-chat/src/client/chat/ApprovalCommand.tsx` | drift | 已登记 | A0-T7 审批详情按工具结构化展示（重写，docs/07 #25） | 上游若结构化渲染，删本地重写；否则保留并确认未截断原文 |
| `packages/client/ui-chat/src/client/chat/ChatView.tsx` | drift | 已登记 | 类型级修复（第 9 行批次）+ fileImages.labels 传 image.openExternal（第 22 行） | 两项分别按对应行规则同步：上游统一文案则删本地传参，类型批随 React 19 升级重跑 tsc -b |
| `packages/client/ui-chat/src/client/chat/MessageItem.module.css` | drift | 已登记 | A0-2 失败行可展开/复制所需的样式（docs/07 #23） | 上游若提供等价诊断 UI，删本地改动 |
| `packages/client/ui-chat/src/client/chat/MessageItem.tsx` | drift | 已登记 | A0-2 TurnErrorItem 增加可展开的原始诊断与复制（docs/07 #23） | 上游若已提供等价原始诊断，删本地改动改跟上游 |
| `packages/client/ui-chat/src/client/chat/TurnTriggerNodeView.tsx` | drift | 已登记 | A0-4 未知时刻改用 span、不挂 dateTime（docs/07 #24） | 同步后确认 time=0 不再渲染成 1970 |
| `packages/client/ui-chat/src/client/chat/flow-motion.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/chat/message-chrome.ts` | drift | 已登记 | A0-4 time<=0 返回「记录时刻不可用」（docs/07 #24） | 同步后确认旧档案不再显示假时钟 |
| `packages/client/ui-chat/src/client/chat/searchable-hidden.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/chat/text-interaction.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/chat/use-chat-scroll.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/chat/use-chat-viewport.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/chat/use-process-scroll.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/contract/slots.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-chat/src/client/conversation-nodes/event-projection.ts` | drift | 已登记 | A0-2 失败节点 detail 透传（docs/07 #23） | 上游若自带 detail，删本地透传 |
| `packages/client/ui-chat/src/client/conversation-nodes/turn-error.ts` | drift | 已登记 | A0-2 TurnErrorNode 增加可选 detail（docs/07 #23） | 上游若等价则删本地字段；字段必须保持可选 |
| `packages/client/ui-chat/src/client/locale.ts` | drift | 已登记 | 8 套词典各加 image.openExternal（共 8 条）（docs/07 #22） | 上游若已加等价键，删本地 8 条改指上游键 |
| `packages/client/ui-chat/tests/approval-command.client.spec.tsx` | drift | 已登记 | A0-T7 详情槽用例（docs/07 #25） | 同步上游后用例随实现一起调整，不要单独删 |
| `packages/client/ui-conversation/src/client/contract/records.ts` | drift | 已登记 | A0-2 记录里的失败节点增加可选 detail（docs/07 #23） | 上游若自带 detail，删本地字段 |
| `packages/client/ui-conversation/src/client/contract/slots.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-conversation/src/client/input/editor/DraftEditor.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-conversation/src/client/input/editor/view-binding.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-conversation/src/client/skeleton/EmptyHero.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-dockkit/src/components/TabLayout.tsx` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（useRef 显式初值），不改运行行为（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批：上游升级 React 19 类型后重跑 tsc -b；上游已修则删本地改动 |
| `packages/client/ui-entry-customization/tests/host-entry.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-entry-customization/tests/menu.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-entry-customization/tests/reader-boot.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-layout/src/client/AppFrame.tsx` | drift | 已登记 | React 19 已按布尔处理 inert，'' 空串写法在新类型下不合法（docs/07 #10） | 同第 9 行类型批；语义等价，上游升级类型后可删 |
| `packages/client/ui-message-feedback/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/FeedbackDialog.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/FeedbackDialog.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/MessageFeedbackActions.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/MessageFeedbackActions.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/controller.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/dialog.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/slots.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/client/surface.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/browser-plugin.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/controller.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/dialog.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/feedback-dialog.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/message-feedback-actions.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tests/styles.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-message-feedback/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-permission-presets/src/client/PermissionSelect.tsx` | drift | 已登记 | 芯片 tooltip 与下拉取文案规则统一（docs/07 #14） | 上游若已统一取文案，删本地函数改调上游 |
| `packages/client/ui-permission-presets/src/client/index.ts` | drift | 已登记 | 新增 presetDetail()：下拉 detail 走词典，自定义挡位才回落宿主描述（docs/07 #13） | 上游若已统一取文案，删本地函数改调上游 |
| `packages/client/ui-permission-presets/src/client/locales.ts` | drift | 已登记 | 8 套词典各加三档说明键（共 24 条），此前宿主下发只有中文（docs/07 #11） | 上游若已把三句收进自身词典，删本地 24 条改指上游键 |
| `packages/client/ui-permission-presets/src/client/presentation.ts` | drift | 已登记 | 新增挡位→词典键映射与回落，集中在一处（docs/07 #12） | 上游若自带等价映射，删本地实现；预设机器值变化需同步 PRESET_DESCRIPTION_KEYS |
| `packages/client/ui-permission-presets/tests/browser-plugin.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-permission-presets/tests/catalog.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-permission-presets/tests/permission-presets-row.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-permission-presets/tests/permission-select.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-permission-presets/tests/settings-store.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-plugin-market/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/DiscoverSection.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/DiscoverSection.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/PluginDetailSection.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/market-store.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/client/presentation.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/browser-plugin.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/discover.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/market-store.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/node-plugin.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/plugin-detail.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tests/presentation.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-plugin-market/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-primitives/src/AnimatedRows.tsx` | drift | 已登记 | 修复 Windows UIA Expand 触发的渲染进程卡死（docs/07 #5） | 上游若修复动画路径重入，删本地补丁 |
| `packages/client/ui-primitives/src/HoverCard.tsx` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（`!== false` → 真值判断）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/JsonTree.tsx` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（useRef 显式初值）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/Menu.tsx` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（`list !== false` → 真值判断）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/TextShimmer.tsx` | drift | 已登记 | React 19 / 严格类型门禁：`inert: ''` → `inert: true`（React 19 已按布尔转发）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/Tooltip.tsx` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（`content !== false` → 真值判断）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/image-drag.ts` | drift | 已登记 | React 19 / 严格类型门禁：类型级修复（useRef 显式初值）（docs/07 #9，2026-11-11 由账本核对补登） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/markdown/MarkdownDelegate.tsx` | drift | 已登记 | 同 render.tsx：fileImages.labels 增加可选 openExternal（docs/07 #21） | 同上 |
| `packages/client/ui-primitives/src/markdown/MarkdownText.module.css` | drift | 已登记 | 新增 .imageAltAction 样式（docs/07 #21） | 同上 |
| `packages/client/ui-primitives/src/markdown/render.tsx` | drift | 已登记 | 失败图片改可点按钮（经宿主 openExternal），避免裸 <a> 把 Tauri 导航到外网（docs/07 #21） | 上游若已做可点失败图片，删本地实现；同步后必须确认仍是按钮/宿主 IPC |
| `packages/client/ui-primitives/src/markdown/useViewportHighlighting.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/useAnchoredMaxHeight.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-primitives/src/useMenuSubmenu.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-session/src/client/index.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-settings-account/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountAvatar.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountAvatar.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountMenu.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountMenu.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountNotice.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountNotice.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountOnboarding.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountPlatformHost.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountQuotaNotice.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountSection.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/AccountSection.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/DesktopOnboarding.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/DesktopOnboarding.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/DesktopOnboardingEntry.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/LogoutIcon.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingConfirmation.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingCreditStep.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingIllustration.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingProcessStep.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingPurposeStep.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingSurface.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingSurface.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/OnboardingWelcomeStep.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/PlatformOverlay.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/PlatformOverlay.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/SignInDialog.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/SignInDialog.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/SignOutDialog.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/account-avatar.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/account-refresh.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-back.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-code.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-compact.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-detailed.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-office.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-recharge-dark.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-recharge-zh-dark.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-recharge-zh.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-recharge.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-standard.svg` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-welcome-dark.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-welcome-zh-dark.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-welcome-zh.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/assets/onboarding-welcome.webp` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/authorize-url.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/bonus-notices.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/browser-login.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/client-metadata.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/contact-url.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/formatBalance.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/locales/onboarding.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/notice-transition.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/onboarding-balance.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/onboarding-contract.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/onboarding-credentials.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/onboarding-state.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/overlay-inert.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/client/platform-pages.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/contact-config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/src/onboarding-settings.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/access-revocation.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/account-avatar.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/account-menu-styles.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/account-refresh.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/account.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/bonus-notices.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/browser-login.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/client-metadata.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/config.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/contact-url.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/desktop-onboarding-entry.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/desktop-onboarding.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/account-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/account-signed-out-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/account-signed-out-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/account-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/avatar-retry.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/bonus-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/bonus-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/details-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/details-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/expired-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/expired-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-active-error-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-active-error-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-copy-failed-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-copy-failed-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-failed-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-no-response-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-no-response-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-timeout-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-timeout-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-waiting-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-waiting-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/login-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/menu-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/menu-signed-out-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/menu-signed-out-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/menu-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/notice.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-credit-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-credit.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-preferences.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-process-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-process.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-purpose-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-purpose.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-welcome-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/onboarding-welcome.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/platform-error-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/platform-error-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/platform-header-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/platform-header-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/profile-before-balance.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-idle-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-idle-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-running-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-running-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-unknown-en.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/expected/sign-out-unknown-zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/formatBalance.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/initialization.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/notice-transition.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/notice.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/onboarding-credentials.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/onboarding-state.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/onboarding-surface.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/onboarding.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/platform-overlay-caption.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/platform-pages.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/quota-notice-hold.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/quota-notice.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/sign-in-dialog.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tests/sign-out-dialog.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-account/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/client/AgentLoopCard.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/client/agent-loop-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/tests/card.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/tests/controller.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-agent-loop/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-general/src/client/SettingsRoot.module.css` | drift | 已登记 | 设置按钮紧凑化并右对齐，纯样式（docs/07 #7） | 上游若已调整为同等形态，删本地补丁 |
| `packages/client/ui-settings-general/src/client/chrome.module.css` | drift | 已登记 | 设置按钮紧凑化并右对齐，纯样式（docs/07 #7） | 上游若已调整为同等形态，删本地补丁 |
| `packages/client/ui-settings-general/src/client/index.ts` | drift | 已登记 | 新增 settingsShell 服务（openSection），供宿主模型配置座位直达设置分段；拆卸链已接 disposeShell（docs/07 #15） | 上游若提供等价入口，删本地服务改调用方；保留时必须确认拆卸链仍调用 disposeShell |
| `packages/client/ui-settings-models/src/client/ModelsSection.module.css` | drift | 已登记 | 「已保存」文案由成功绿改中性色，避免把「写进本机」读成「校验通过」（docs/07 #27） | 上游若自己区分「已保存」与「已校验」，删本地改动；保留时确认 .savedNotice 没有退回 success 色 |
| `packages/client/ui-settings-models/src/client/ModelsSection.tsx` | drift | 已登记 | 账号行绿点补 role=img + title/aria-label，悬停与读屏都能得到「已登录」（docs/07 #27） | 上游若已给该绿点等价标签，删本地改动改跟上游 |
| `packages/client/ui-settings-models/src/client/locales.ts` | drift | 已登记 | credentialConfigured 的 zh/en 改为「已配置（未校验有效性）」，不再暗示密钥有效（docs/07 #27） | 上游若自己区分「已保存」与「已校验」，删本地文案；保留时确认没有退回「已配置」这种读法 |
| `packages/client/ui-settings-plugin-inventory/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/client/PluginInventorySettingsTab.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/client/PluginInventorySettingsTab.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/tests/browser-plugin.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/tests/components.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/tests/touch-styles.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugin-inventory/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/client/PluginsSettingsSection.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/client/PluginsSettingsSection.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/tests/section.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-plugins/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/client/UploadRow.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/client/UploadRow.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/client/upload-preference.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/tests/expected/upload-row.zh.txt` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/tests/upload.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-session-log/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/client/ShellCard.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/client/shell-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/tests/card.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/tests/controller.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-shell/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentCard.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentCard.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentLimitsFields.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentLimitsFields.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentModelSelectionFields.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/SubagentModelSelectionFields.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/subagent-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/subagent-limits-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/client/subagent-model-selection-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/tests/card.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/tests/controllers.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-subagent/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/README.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/package.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/client/WebSearchCard.module.css` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/client/WebSearchCard.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/client/web-search-card-controller.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tests/apply.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tests/card.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tests/controller.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tests/styles.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-settings-web-search/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（设置只留通用 + 模型，见 docs/07 #19） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/README.i18n.yaml` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/README.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/README.zh.md` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/de.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/en.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/es.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/fr.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/ja.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/pt.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/ru.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/locale/zh.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/package.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/Editor.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/Icons.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/Reference.module.css` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/Reference.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/feedback.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/fixed.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/locales.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/client/store.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/css-modules.d.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/src/index.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/tests/commands.client.spec.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/tests/editor.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/tests/reference.client.spec.tsx` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/tsconfig.json` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-shortcuts/tsdown.config.ts` | not-applicable | 已登记 | 整包未 vendor（本项目未接线该能力） | 接入该能力时整包 vendor，并按 docs/07 登记改动 |
| `packages/client/ui-sidebar-browser/src/client/view/BrowserBody.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-documentpreview/src/client/image/ImageBody.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-documentpreview/src/client/pdf/pdf.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-documentpreview/src/client/zoom/ZoomViewport.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-right/src/client/shell/SidebarRight.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-right/src/client/tab-domain.ts` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-sidebar-terminal/src/client/TerminalCleanup.module.css` | drift | 已登记 | 同上，bg-l2 → bg-layer-2（docs/07 #4） | 同上 |
| `packages/client/ui-sidebar-terminal/src/client/TerminalCleanup.tsx` | drift | 已登记 | 终端清理面板改走本地 ./model.ts 契约（docs/07 #3） | 同 face.ts |
| `packages/client/ui-sidebar-terminal/src/client/TerminalGuide.tsx` | drift | 已登记 | 终端引导面板改走本地 ./model.ts 契约（docs/07 #3） | 同 face.ts |
| `packages/client/ui-sidebar-terminal/src/client/TerminalTitle.module.css` | drift | 已登记 | 同上，bg-l1 → bg-layer-1（docs/07 #4） | 同上 |
| `packages/client/ui-sidebar-terminal/src/client/face.ts` | drift | 已登记 | 上游依赖未 vendor 的 dsh-api-terminal-controller，导入改指向本地 ./model.ts（docs/07 #3） | 上游若提供 browser-safe 控制器契约，改回上游导入并删 model.ts |
| `packages/client/ui-sidebar-terminal/src/client/index.ts` | drift | 已登记 | 同 face.ts 改本地导入；target() 收窄为只接受终端形状参数（docs/07 #3） | 同上；上游若已把参数收窄，删本地改动 |
| `src/dsh/packages/client/ui-sidebar-terminal/src/client/model.ts` | not-applicable | 已登记 | 本地新增（约 146 行）：控制器包依赖宿主 PTY 未 vendor，自实现同一契约 | 上游若提供 browser-safe 控制器包，删本地实现改导入上游 |
| `packages/client/ui-sidebar-terminal/src/client/terminal.module.css` | drift | 已登记 | 上游包间令牌漂移：ui-theme 已改 bg-layer-*，本包仍引用 bg-l1/-2 导致面板透明（docs/07 #4） | 同步上游时先看 terminal 是否已改用 bg-layer-*，已改则删本地补丁；src/theme-tokens.spec.ts 机检兜底 |
| `packages/client/ui-sidebar-terminal/src/client/terminal.tsx` | drift | 已登记 | 终端会话实现改走本地 ./model.ts 契约（docs/07 #3） | 同 face.ts |
| `packages/client/ui-sidebar-terminal/tests/apply.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/bundle-split.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/cleanup.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/recovery.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/terminal-body.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/terminal-cursor.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/terminal-guide.client.spec.tsx` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tests/terminal-theme.client.spec.ts` | not-applicable | 已登记 | 上游测试不随包 vendor；本项目用例另写 | 同步上游时对照上游用例核对行为差异，必要时补本地用例 |
| `packages/client/ui-sidebar-terminal/tsdown.config.ts` | not-applicable | 已登记 | 包构建入口不进本项目构建链 | 无需同步 |
| `packages/client/ui-theme/src/client/styles.ts` | drift | 已登记 | 8 张主题样式表改构造式样式表，绕过打包后 CSP style-src 拦截（docs/07 #20） | 上游若改构造式样式表或给 <style> 打 nonce，删本地实现；不要退回 <style> 注入 |
| `packages/client/ui-theme/tests/client-styles.client.spec.ts` | drift | 已登记 | 用例断言对象改为 adoptedStyleSheets，配合上面的实现改动（docs/07 #20） | 实现回退时用例一并回退 |
| `packages/client/ui-workflow-run/src/client/WorkflowRunPanel.tsx` | drift | 已登记 | 类型级修复（docs/07 #9） | 同第 9 行类型批 |
| `packages/client/ui-workspace/src/client/contract/slots.ts` | drift | 已登记 | SH-113 搜索失败/残缺与重试注入（docs/07 #26） | 上游若自己区分搜索失败与残缺，删本地字段 |
| `packages/client/ui-workspace/src/client/index.ts` | drift | 已登记 | SH-113 注入 retrySearchIndex（docs/07 #26） | 同步后确认注入仍是可选 |
| `packages/client/ui-workspace/src/client/locales.ts` | drift | 已登记 | SH-113 search.failed/.skipped/.retry 词典键（docs/07 #26） | 上游若自带等价键，删本地键改指上游 |
| `packages/client/ui-workspace/src/client/rows/WorkspaceBrowser.module.css` | drift | 已登记 | SH-113 失败/残缺两行样式（docs/07 #26） | 上游若自带，删本地样式 |
| `packages/client/ui-workspace/src/client/rows/WorkspaceBrowser.tsx` | drift | 已登记 | 同 AnimatedRows：修 UIA Expand 卡死并接入本地键盘模型（docs/07 #5） | 上游若修复重入且自带键盘模型，删本地改动 |
| `src/dsh/packages/client/ui-workspace/src/client/rows/treeKeyboard.ts` | not-applicable | 已登记 | 本地新增：ARIA 树键盘模型（上游只有鼠标展开/收起，见 docs/07 #5） | 上游若自带键盘模型，删本地实现改跟上游 |

## 上游其它 packages（本批未覆盖，仅统计规模）

| 分组 | 文件数 |
| --- | --- |
| packages/acp | 35 |
| packages/api | 383 |
| packages/attachment | 49 |
| packages/boot | 198 |
| packages/browser-use | 19 |
| packages/bundle | 116 |
| packages/compaction | 92 |
| packages/computer-use | 19 |
| packages/context | 128 |
| packages/core | 259 |
| packages/credentials | 84 |
| packages/deliverables | 66 |
| packages/document | 26 |
| packages/experimental | 2318 |
| packages/extensions | 128 |
| packages/feedback | 38 |
| packages/fs | 167 |
| packages/goal | 77 |
| packages/guard | 33 |
| packages/host | 164 |
| packages/identity | 10 |
| packages/interaction | 95 |
| packages/jobs | 55 |
| packages/llm | 292 |
| packages/lsp | 70 |
| packages/mcp | 86 |
| packages/plan | 23 |
| packages/preset | 100 |
| packages/ptc-runtime | 83 |
| packages/sandbox | 95 |
| packages/schedule | 71 |
| packages/sdk | 71 |
| packages/session | 459 |
| packages/session-query | 82 |
| packages/settings | 28 |
| packages/shell | 179 |
| packages/skill | 99 |
| packages/spill | 51 |
| packages/ssh | 106 |
| packages/storage | 79 |
| packages/subagent | 267 |
| packages/subprocess | 88 |
| packages/team | 50 |
| packages/telemetry | 23 |
| packages/terminal | 42 |
| packages/test-support | 225 |
| packages/todo | 23 |
| packages/typert | 118 |
| packages/util | 193 |
| packages/web | 109 |
| packages/workflow | 63 |
| packages/workspace | 22 |
