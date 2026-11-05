/**
 * SHANIU 的 DSH 静态组装入口（替换 DSH 的 manifest 扫描 + bundle loader）。
 * 组装范式照抄 DSH 官方测试样板 packages/client/ui-layout/tests/apply.client.spec.ts：
 * Context → SlotRegistry（随 renderer apply）→ locale → shortcuts → 各特性包 {inject, apply}。
 * 宿主侧服务（sessions/remote/configForms/remoteAccess）由 src/transport/ 适配层提供；
 * 尚未接线的能力一律经 stubs.ts 显式 stub，不做隐式降级。
 */
import { Context } from '@deepseek-ai/cordis'
import { apply as rendererApply } from '@deepseek-ai/dsh-client-ui-renderer/client'
import { apply as themeApply, inject as themeInject } from '@deepseek-ai/dsh-client-ui-theme/client'
import { apply as layoutApply, inject as layoutInject } from '@deepseek-ai/dsh-client-ui-layout/client'
import { apply as uiSessionApply, inject as uiSessionInject } from '@deepseek-ai/dsh-client-ui-session/client'
import { apply as uiWorkspaceApply, inject as uiWorkspaceInject } from '@deepseek-ai/dsh-client-ui-workspace/client'
import { apply as uiSidebarApply, inject as uiSidebarInject } from '@deepseek-ai/dsh-client-ui-sidebar/client'
import { apply as uiSidebarRightApply, inject as uiSidebarRightInject } from '@deepseek-ai/dsh-client-ui-sidebar-right/client'
import { apply as uiConversationApply, inject as uiConversationInject } from '@deepseek-ai/dsh-client-ui-conversation/client'
import { apply as uiChatApply, inject as uiChatInject } from '@deepseek-ai/dsh-client-ui-chat/client'
import { apply as uiApprovalApply, inject as uiApprovalInject } from '@deepseek-ai/dsh-client-ui-approval/client'
import { apply as uiToolApply, inject as uiToolInject } from '@deepseek-ai/dsh-client-ui-tool/client'
import { apply as uiTrajectoryApply, inject as uiTrajectoryInject } from '@deepseek-ai/dsh-client-ui-trajectory/client'
import { apply as uiUserQuestionsApply, inject as uiUserQuestionsInject } from '@deepseek-ai/dsh-client-ui-user-questions/client'
import { apply as uiModelSelectionApply, inject as uiModelSelectionInject } from '@deepseek-ai/dsh-client-ui-model-selection/client'
import { apply as uiPlanApply, inject as uiPlanInject } from '@deepseek-ai/dsh-client-ui-plan/client'
import { apply as uiDeliverablesApply, inject as uiDeliverablesInject } from '@deepseek-ai/dsh-client-ui-deliverables/client'
import { apply as uiAttachmentApply, inject as uiAttachmentInject } from '@deepseek-ai/dsh-client-ui-attachment/client'
import { apply as uiContextMenuApply, inject as uiContextMenuInject } from '@deepseek-ai/dsh-client-ui-context-menu/client'
import { apply as uiFileDragApply, inject as uiFileDragInject } from '@deepseek-ai/dsh-client-ui-file-drag/client'
import { apply as uiInputTriggerApply, inject as uiInputTriggerInject } from '@deepseek-ai/dsh-client-ui-input-trigger/client'
import { apply as uiCommandsApply, inject as uiCommandsInject } from '@deepseek-ai/dsh-client-ui-commands/client'
import { apply as fileUploadApply, inject as fileUploadInject } from '@deepseek-ai/dsh-client-file-upload/client'
import { apply as resourcesApply, inject as resourcesInject } from '@deepseek-ai/dsh-client-resources/client'
import { apply as workspaceControllerApply, inject as workspaceControllerInject } from '@deepseek-ai/dsh-api-workspace-controller/client'
import ShortcutsService from '@deepseek-ai/dsh-client-shortcuts/client'
import { apply as localeApply, inject as localeInject } from '@deepseek-ai/dsh-client-locale/client'
import {
  StubRemoteAccess, installRemoteNamespaceStubs,
  stubConfigForms, stubRemote, stubSessions,
} from './transport/stubs'

export interface BootHandle {
  readonly ctx: Context
  dispose(): Promise<void>
}

/** 按依赖序安装一个 {inject, apply} 特性插件。 */
type FeaturePlugin = { inject: readonly string[]; apply: (ctx: Context) => unknown }

/** 组装并挂载完整 UI；失败时把原因抛给调用方（入口页自绘错误）。 */
export async function bootShaniu(container: HTMLElement): Promise<BootHandle> {
  const root = new Context()

  // 渲染基座：SlotRegistry + slot renderer + ctx.uiRenderer.mount
  await root.plugin({ inject: [], apply: rendererApply }).await()

  // 宿主侧服务 stub（M0：尚未接线的显式占位，清单见 transport/stubs.ts）
  root.provide('remote', stubRemote())
  installRemoteNamespaceStubs(root)
  root.provide('configForms', stubConfigForms())
  root.provide('sessions', stubSessions())
  new StubRemoteAccess(root) // Service 子类：构造即自注册 'remoteAccess'

  // 基础服务：locale 插件（词典注册 + slots.installLocale，渲染机合成 t 座席）
  // → shortcuts（依赖 locale）→ theme（依赖 slots/locale/remote/configForms）
  await root.plugin({ inject: [...localeInject], apply: localeApply }).await()
  await root.plugin(ShortcutsService).await()
  await root.plugin({ inject: [...themeInject], apply: themeApply }).await()

  // session 作用域适配器：AppFrame 的 'session-maybe' 出口渲染必需
  await root.plugin({ inject: [...uiSessionInject], apply: uiSessionApply }).await()

  // batch-2 特性包（按依赖序；每个包的 inject 见其 apply 模块）：
  // 资源/上传/拖拽/右键菜单/输入触发/命令 → 工作区 → 会话主区 → 右栏/侧栏
  const features: FeaturePlugin[] = [
    { inject: resourcesInject, apply: resourcesApply },
    { inject: fileUploadInject, apply: fileUploadApply },
    { inject: uiFileDragInject, apply: uiFileDragApply },
    { inject: uiContextMenuInject, apply: uiContextMenuApply },
    { inject: uiInputTriggerInject, apply: uiInputTriggerApply },
    { inject: uiCommandsInject, apply: uiCommandsApply },
    { inject: workspaceControllerInject, apply: workspaceControllerApply },
    { inject: uiWorkspaceInject, apply: uiWorkspaceApply },
    { inject: uiConversationInject, apply: uiConversationApply },
    { inject: uiSidebarRightInject, apply: uiSidebarRightApply },
    { inject: uiSidebarInject, apply: uiSidebarApply },
    { inject: uiChatInject, apply: uiChatApply },
    { inject: uiApprovalInject, apply: uiApprovalApply },
    { inject: uiToolInject, apply: uiToolApply },
    { inject: uiTrajectoryInject, apply: uiTrajectoryApply },
    { inject: uiUserQuestionsInject, apply: uiUserQuestionsApply },
    { inject: uiModelSelectionInject, apply: uiModelSelectionApply },
    { inject: uiPlanInject, apply: uiPlanApply },
    { inject: uiDeliverablesInject, apply: uiDeliverablesApply },
    { inject: uiAttachmentInject, apply: uiAttachmentApply },
  ]
  for (const feature of features) {
    await root.plugin({ inject: [...feature.inject], apply: feature.apply }).await()
  }

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
