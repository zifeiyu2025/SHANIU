/**
 * SHANIU 自有模型席位插件（宿主侧，非 vendored）：没有任何可用提供商时，
 * 把输入框的模型席位换成「模型配置」，点击直达设置里的模型页。
 *
 * 之所以要接管而不是只换文案：官方席位在空目录时仍展开空菜单（「暂无可用模型」），
 * 而产品要求这一下直接去配置。接缝用的是 `conversation.input.model` 这个单占位槽——
 * 以 priority -1 影子上游注册（框架规定同槽「低优先级渲染」），配置好提供商后再撤回，
 * 席位自动交还上游的模型选择器。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import type {} from '@deepseek-ai/dsh-client-ui-model-selection/client'
import { ModelConfigSeat } from './ModelConfigSeat'
import { en, NS, zh } from './locales'

/** 必需服务：槽位、词典、远端面（读提供商目录、订阅配置变化）与设置面板的按段打开入口。 */
export const inject = ['slots', 'locale', 'remote', 'settingsShell']

declare module '@deepseek-ai/cordis' {
  interface Context {
    /** 设置面板暴露的「按分段打开」入口；面板未挂载时不存在。 */
    settingsShell?: { openSection(id: string): void }
  }
}

/** 设置里模型分段注册的 id（ui-settings-models 的 `settings.section`）。 */
const MODELS_SECTION = 'models'

/**
 * 注册席位影子：未配置时接管，配置好后撤回。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-model-config: dictionaries')
  const open = (): void => { ctx.settingsShell?.openSection(MODELS_SECTION) }

  ctx.slots.inject('conversation.input.model', () => {
    let dispose: (() => void) | undefined
    let seated = false

    const sync = async (): Promise<void> => {
      const providers = await ctx.remote.llm?.listProviders()
      // 远端面缺失时按"已配置"处理，避免把正常席位顶掉
      const unconfigured = providers !== undefined && providers.ok && providers.value.length === 0
      if (unconfigured === seated) return
      seated = unconfigured
      if (unconfigured) {
        dispose = ctx.slots.register({
          name: 'conversation.input.model',
          priority: -1,
          locale: NS,
          inject: () => ({ open }),
        }, ModelConfigSeat)
      } else {
        dispose?.()
        dispose = undefined
      }
    }

    void sync()
    const offs = [
      // 与上游模型目录同一组失效事件：设置写入、适配器变化、凭据变化
      ctx.remote.$on('settings/document-updated', () => { void sync() }),
      ctx.remote.$on('llm/adapters-updated', () => { void sync() }),
      ctx.remote.$on('llm/models-updated', () => { void sync() }),
      ctx.remote.$on('credentials/reference-updated', () => { void sync() }),
    ]
    return () => {
      for (const off of offs) off()
      dispose?.()
      dispose = undefined
    }
  })
}
