/**
 * SHANIU 自有 MCP 连接器插件（宿主侧，非 vendored）。
 *
 * 落位（临时双挂载）：
 * - `plugins.item` 是**唯一正确位置**（插件页官方条目组）：卡片标题「MCP 连接器」，
 *   摘要行是状态聚合；点开进入上游详情页，页内「服务器 / 豁免」两个 tab。外壳
 *   （卡片、列表项、详情页、返回、面包屑）全部由插件页提供。
 * - 宿主当前把插件页整体置为 unavailable（`managementAvailable: false`，见
 *   `src/transport/adapter.ts`），卡片渲染不出来。为此临时加一个
 *   `settings.section` 挂载点，**挂的是同一个 `McpConnector` 组件定义**：一份 UI、
 *   两个挂载点，数据源同一份（`mcp-wire` 的宿主命令 + `shaniu://mcp` 事件）、
 *   状态同一份，不存在两套列表，也不会状态分叉。
 * - 这与被否掉的「两套列表 UI」（A+B）不同：**不是复制组件，也不是两套数据流**，
 *   只是同一组件的第二个挂载点。不要为了让它显示而伪报 `managementAvailable`。
 * - **删除条件（写死）**：宿主 `managementAvailable` 变 true 且插件页能正常渲染
 *   卡片后，删除 `settings.section` 这条注册。
 *
 * 数据面是 `src/transport/mcp-wire.ts` 的宿主命令与 `shaniu://mcp` 应用级事件。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-plugin-manager/client'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-slots'
import { McpConnector } from './McpConnector'
import { en, NS, zh } from './locales'

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
  // 临时兜底挂载点：同一个 `McpConnector` 定义，宿主补齐插件管理能力后删除。
  ctx.slots.inject('settings.section', () => ctx.slots.register({
    name: 'settings.section',
    id: 'mcp',
    order: 30,
    label: () => t('label'),
    locale: NS,
  }, McpConnector))
}
