/**
 * SHANIU 自有 MCP 信任设置页插件（宿主侧，非 vendored）。
 *
 * 只注册一个 `settings.section`：首连审查、逐工具风险、撤销信任/豁免都在
 * `McpTrustSection` 里；数据面是 `src/transport/mcp-wire.ts` 的 5 个宿主命令
 * 与 `shaniu://mcp` 应用级事件。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-slots'
import { McpTrustSection } from './McpTrustSection'
import { en, NS, zh } from './locales'

/** 必需服务：设置面板槽位与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册 MCP 信任分段。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-mcp-trust: dictionaries')
  const t = ctx.locale.bind(NS)
  ctx.slots.inject('settings.section', () => ctx.slots.register({
    name: 'settings.section',
    id: 'mcp',
    order: 20,
    label: () => t('nav'),
    locale: NS,
  }, McpTrustSection))
}
