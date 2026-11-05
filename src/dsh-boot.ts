/**
 * SHANIU 的 DSH 静态组装入口（替换 DSH 的 manifest 扫描 + bundle loader）。
 * 组装范式照抄 DSH 官方测试样板 packages/client/ui-layout/tests/apply.client.spec.ts：
 * Context → SlotRegistry（随 renderer apply）→ locale → shortcuts → 各特性包 {inject, apply}。
 * 宿主侧服务（sessions/remote/configForms/connection）由 src/transport/ 适配层提供；
 * 尚未接线的能力一律经 stubs.ts 显式 stub，不做隐式降级。
 */
import { Context } from '@deepseek-ai/cordis'
import { apply as rendererApply } from '@deepseek-ai/dsh-client-ui-renderer/client'
import { apply as themeApply, inject as themeInject } from '@deepseek-ai/dsh-client-ui-theme/client'
import { apply as layoutApply, inject as layoutInject } from '@deepseek-ai/dsh-client-ui-layout/client'
import { apply as uiSessionApply, inject as uiSessionInject } from '@deepseek-ai/dsh-client-ui-session/client'
import ShortcutsService from '@deepseek-ai/dsh-client-shortcuts/client'
import { apply as localeApply, inject as localeInject } from '@deepseek-ai/dsh-client-locale/client'
import {
  StubRemoteAccess, stubConfigForms, stubRemote, stubSessions,
} from './transport/stubs'

export interface BootHandle {
  readonly ctx: Context
  dispose(): Promise<void>
}

/** 组装并挂载完整 UI；失败时把原因抛给调用方（入口页自绘错误）。 */
export async function bootShaniu(container: HTMLElement): Promise<BootHandle> {
  const root = new Context()

  // 渲染基座：SlotRegistry + slot renderer + ctx.uiRenderer.mount
  await root.plugin({ inject: [], apply: rendererApply }).await()

  // 宿主侧服务 stub（M0：尚未接线的显式占位，清单见 transport/stubs.ts）
  root.provide('remote', stubRemote())
  root.provide('configForms', stubConfigForms())
  root.provide('sessions', stubSessions())
  new StubRemoteAccess(root) // Service 子类：构造即自注册 'remoteAccess'

  // 基础服务：locale 插件（词典注册 + slots.installLocale，渲染机合成 t 座席）
  // → shortcuts（依赖 locale）→ theme（依赖 slots/locale/remote/configForms）
  await root.plugin({ inject: [...localeInject], apply: localeApply }).await()
  await root.plugin(ShortcutsService).await()
  await root.plugin({ inject: [...themeInject], apply: themeApply }).await()

  // session 作用域适配器：AppFrame 的 'session-maybe' 出口渲染必需（ui-session 注入
  // sessions/slots/remote/remoteAccess 四个面，前三者已就位）
  await root.plugin({ inject: [...uiSessionInject], apply: uiSessionApply }).await()

  // 布局壳：占据 root slot（sidebar/main/rightbar/shell.*）
  await root.plugin({ inject: [...layoutInject], apply: layoutApply }).await()

  // 挂载：等价 DSH mountClient —— 等 uiRenderer 就位后 mount
  const uiRenderer = root.get('uiRenderer')
  if (uiRenderer === undefined) throw new Error('dsh-boot: uiRenderer 服务未就位')
  const unmount = uiRenderer.mount(container)

  return {
    ctx: root,
    async dispose() {
      unmount()
      await root.fiber.dispose()
    },
  }
}
