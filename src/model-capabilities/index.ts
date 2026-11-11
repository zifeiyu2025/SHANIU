/**
 * SH-132 能力视图的宿主插件：把「值 + 来源 + 实测三态」挂到设置页的服务商卡片上。
 *
 * 走的是上游 `settings.models.provider-card` 扩展点（`slot-contract.ts` 明说这是
 * "外部插件不改模型设置页也能加 UI"的座位），键控于 `settingsNs`。我们的模型页挂在
 * `llm-pi-ai`（见 `transport/settings-face.ts` 的 `viewOfPi`），所以每个服务商卡片
 * 都会渲染一次本区块。
 *
 * 为什么不改 vendored 的 ModelRow：那是"先改通再说"的做法，跟上游同步时必然丢；
 * 上游本来就没有模型行级 slot，但提供了 provider-card 级座位，够用就不动上游文件。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-settings-models/client'
import { CapabilityPanel } from './CapabilityPanel'
import { en, NS, zh } from './locales'
import { hostCapabilitiesWire } from './wire'

/** 必需服务：槽位、词典、远端事件（设置写入后重读）。 */
export const inject = ['slots', 'locale', 'remote']

/** 我们的模型设置页所在的设置命名空间（settings-face 的 `viewOfPi`）。 */
const SETTINGS_NS = 'llm-pi-ai'

/**
 * 注册服务商卡片里的能力区块。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-model-capabilities: dictionaries')
  const wire = hostCapabilitiesWire()

  ctx.slots.inject('settings.models.provider-card', () => ctx.slots.register({
    name: 'settings.models.provider-card',
    key: SETTINGS_NS,
    locale: NS,
    inject: () => ({
      wire,
      // 设置保存后宿主会广播两类事件，含义不同，不能只订阅一类：
      //  - `settings/document-updated`：能力来源（手动/内置表）与端点地址会变，要重读；
      //  - `credentials/reference-updated`：密钥刚被保存/更换（`settings-face.ts` 的
      //    `credentials.set` 成功后发出），**不会**同时发文档事件，只重读文档会让
      //    三态停在保存前的旧结论上。
      subscribe: (handler: (event?: string) => void) => {
        const offDocument = ctx.remote.$on('settings/document-updated', () => handler('settings/document-updated'))
        const offCredential = ctx.remote.$on('credentials/reference-updated', () => handler('credentials/reference-updated'))
        return () => {
          offDocument()
          offCredential()
        }
      },
    }),
  }, CapabilityPanel))
}
