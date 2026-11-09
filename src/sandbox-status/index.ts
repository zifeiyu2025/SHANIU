/**
 * SH-021 沙箱可见性插件（宿主侧，非 vendored）。
 *
 * 红线背景（AGENTS §11.3）：「不静默降级」只有在用户看得见时才有意义。
 * 本插件把可见面补在 `shell.bottom`（根作用域），但**只在异常时出现**：
 * `enforcement = partial`（逻辑沙箱，文件限制未全强制）或状态读取失败。
 * 档位措辞归输入区的访问模式 chip（DSH PermissionSelect），这里不重复。
 *
 * **没有会话级网络开关**（2026-11-10 定，对齐 DSH）：网络不是策略维度，容器一律可出网。
 * 曾经挂在 `conversation.session.header.utilities` 的「允许本会话联网」已随网络闸一起删除。
 * 因此这里只保留一个根作用域槽位——它拿不到当前会话 id，也不需要。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import { NS, en, zh } from './locales'
import { SandboxStatusBar } from './SandboxStatusBar'

/** 必需服务：UI 槽位注册表与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册沙箱异常提示（`shell.bottom`，常态不渲染）。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-sandbox: dictionaries')

  ctx.slots.inject('shell.bottom', () => ctx.slots.register({
    name: 'shell.bottom',
    locale: NS,
  }, SandboxStatusBar))
}
