/**
 * SH-021 沙箱可见性插件（宿主侧，非 vendored）。
 *
 * 红线背景（AGENTS §11.3）：「不静默降级」只有在用户看得见时才有意义。
 * 后端 `sandbox_status` / 网络授权四命令自 SH-021 起就有了，但前端一直没有呈现；
 * 本插件把可见面补成两个槽位：
 *
 *  - `shell.bottom`（常驻，根作用域）：模式 / OS 机制 / 容器 profile / 网络总体状态；
 *  - `conversation.session.header.utilities`（会话内）：本会话的联网批准与撤销。
 *
 * 拆成两处而不是塞一处，是因为"常驻"与"知道当前会话"不能同时满足：根作用域槽位
 * 拿不到用户正在看的会话 id（`retainedBy.mainView` 在新建草稿时会指向历史会话），
 * 会话级操作放那里就会批准到错误的会话上。
 */
import type { Context } from '@deepseek-ai/cordis'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import { NS, en, zh } from './locales'
import { NetworkToggle } from './NetworkToggle'
import { SandboxStatusBar } from './SandboxStatusBar'

/** 必需服务：UI 槽位注册表与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册底部状态条与会话头部网络开关。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-sandbox: dictionaries')

  ctx.slots.inject('shell.bottom', () => ctx.slots.register({
    name: 'shell.bottom',
    locale: NS,
  }, SandboxStatusBar))

  ctx.slots.inject('conversation.session.header.utilities', () => ctx.slots.register({
    name: 'conversation.session.header.utilities',
    id: 'shaniu-network-toggle',
    order: -20,
    locale: NS,
    inject: (sessionId: SessionId) => ({ sessionId }),
  }, NetworkToggle))
}
