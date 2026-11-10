/**
 * 退出守卫：关闭窗口收托盘，真正退出前先查有没有在跑的任务。
 *
 * 为什么要独立成插件：托盘菜单（宿主事件）与「更多操作 → 退出 SHANIU」
 * （会话头菜单）必须落到同一个对话框与同一套判断，不能各写一份。
 */
import type { Context } from '@deepseek-ai/cordis'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import { tauriBridge } from '../transport/tauri'
import { en, NS, zh } from './locales'
import { QuitDialog } from './QuitDialog'

/** 必需服务：overlay 槽位、词典、会话列表（解析会话标题）。 */
export const inject = ['slots', 'locale', 'sessions']

/**
 * 注册退出对话框。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-quit: dictionaries')
  const titleOf = (sessionId: string): string =>
    ctx.sessions.list.getSnapshot().byId[sessionId as SessionId]?.displayTitle ?? sessionId
  const bridge = tauriBridge()
  ctx.slots.inject('shell.overlay', () => ctx.slots.register({
    name: 'shell.overlay',
    id: 'shaniu-quit-guard',
    order: 110,
    locale: NS,
    inject: () => ({
      titleOf,
      cancelAndQuit: (): Promise<void> => bridge.invoke('quit_after_cancel').then(() => undefined),
      hideToTray: (): Promise<void> => bridge.invoke('hide_main_window').then(() => undefined),
    }),
  }, QuitDialog))
}
