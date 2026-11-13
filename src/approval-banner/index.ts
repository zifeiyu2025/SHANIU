/**
 * 全局待审批横幅插件（宿主侧，非 vendored）。
 *
 * 只注册一个 `shell.overlay` 占用者：没有待审批时整条不渲染，所以常态零干扰。
 * 不占用 `shell.bottom`——那里是沙箱提示的单一占用位（`single` 槽位，后注册会顶掉前者）。
 *
 * 定位链路：横幅行自带 `sessionId` → `uiWorkspace.openSession` 切过去 →
 * 扫描 `[data-approval-key]` 聚焦（vendored `ApprovalCard` 已有该锚点，零改动）。
 */
import type { Context } from '@deepseek-ai/cordis'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-workspace/client'
import { NS, en, zh } from './locales'
import { ApprovalBanner } from './ApprovalBanner'
import { currentApprovalCard, focusApprovalCard } from './focus'

/** 必需服务：overlay 槽位、词典、会话列表（解析标题）、跨会话导航。 */
export const inject = ['slots', 'locale', 'sessions', 'uiWorkspace']

/**
 * 注册待审批横幅。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-approval: dictionaries')
  const titleOf = (sessionId: string): string =>
    ctx.sessions.list.getSnapshot().byId[sessionId as SessionId]?.displayTitle ?? sessionId

  ctx.slots.inject('shell.overlay', () => ctx.slots.register({
    name: 'shell.overlay',
    id: 'shaniu-approval-banner',
    // 退出对话框是 110：横幅在它下面，避免盖住"还有事没做完"的追问。
    order: 100,
    locale: NS,
    inject: () => ({
      titleOf,
      openSession: (sessionId: string): void => { ctx.uiWorkspace.openSession(sessionId as SessionId) },
      // 点击瞬间先记下旧卡：会话切换完成后，旧卡会被新会话的卡替换。
      focusApproval: (): void => { focusApprovalCard(document, currentApprovalCard(document)) },
    }),
  }, ApprovalBanner))
}
