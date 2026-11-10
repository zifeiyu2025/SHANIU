/**
 * SHANIU 自有 MCP 连接器插件（宿主侧，非 vendored）。
 *
 * 落位：`plugins.item`（插件页官方条目组）。卡片标题「MCP 连接器」，摘要行是状态聚合；
 * 点开进入上游详情页（`ItemDetail` 自带 `h3` 标题，外层插件段有 `h1`），页内
 * 「服务器 / 豁免」两个 tab。外壳（卡片、列表项、详情页、返回、面包屑）全部由插件页提供。
 *
 * 2026-11-10（v0.2.1 视觉对齐批）：删除过渡期的 `settings.section` 临时挂载点。
 * 该挂载点渲染时没有 `h2`（组件正文以说明段落开头），在设置侧栏里是一页没有标题的
 * 裸页；插件页的 `ItemDetail` 本身提供标题层级，回到插件段才是正确落位。
 *
 * 数据面是 `src/transport/mcp-wire.ts` 的宿主命令与 `shaniu://mcp` 应用级事件。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-plugin-manager/client'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-slots'
import { McpConnector } from './McpConnector'
import { en, NS, zh } from './locales'
import { GeneralConfigReadOnlyRow } from '../config-readonly/ConfigReadOnly'

/** 必需服务：槽位与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册 MCP 连接器条目（一处 UI，两个挂载点）。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-mcp-trust: dictionaries')
  const t = ctx.locale.bind(NS)
  ctx.slots.inject('plugins.item', () => ctx.slots.register({
    name: 'plugins.item',
    id: 'mcp',
    order: 30,
    label: () => t('label'),
    locale: NS,
  }, McpConnector))
  // 配置只读是全局状态：通用设置页也挂同一条横幅（同一组件、同一词典键），
  // 只在 MCP 页显示会让人以为「只有 MCP 坏了」，比没有提示更误导。
  ctx.slots.inject('settings.general.item', () => ctx.slots.register({
    name: 'settings.general.item',
    id: 'config-readonly',
    order: -100,
    locale: NS,
  }, GeneralConfigReadOnlyRow))
}
