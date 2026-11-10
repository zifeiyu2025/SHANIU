/**
 * 托盘提示（SH-072 方案 A）：关窗口只是隐藏到托盘，仍有终端在跑时必须让人知道，
 * 否则「我关了窗口应该没事了」的误判会让人白等一个还在运行的进程。
 *
 * 宿主在 `CloseRequested`（main 窗口）里读一次存活会话数，>0 才通过
 * `user-terminal://tray-hint` 发一次；这里只把它渲染成一次性 toast——
 * 不弹模态、不重试、不轮询。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import { en, NS, zh } from './locales'
import { TrayHintToast } from './TrayHintToast'

/** 必需服务：UI 槽位注册表（`shell.overlay` 由 Chat 宿主声明）与词典。 */
export const inject = ['slots', 'locale']

/**
 * 把提示组件挂进 frame 级 overlay。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-tray-hint: dictionaries')
  ctx.slots.inject('shell.overlay', () => ctx.slots.register({
    name: 'shell.overlay',
    id: 'shaniu-tray-hint',
    order: 100,
    locale: NS,
  }, TrayHintToast))
}
