/**
 * 「默认目录」说明行（宿主插件）。
 *
 * 挂载点：`settings.general.item`。默认目录是应用级路径事实，不属于某个功能页，
 * 放在通用设置里与相邻的只读提示同一处；不改 vendored 工作区组件。
 */
import type { Context } from '@deepseek-ai/cordis'
import { DefaultDirRow } from './DefaultDirRow'
import { NS, en, zh } from './locales'

/** 必需服务：槽位与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册通用设置页的「默认目录」一行。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-default-dir: dictionaries')
  ctx.slots.inject('settings.general.item', () => ctx.slots.register({
    name: 'settings.general.item',
    id: 'default-dir',
    // 排在配置只读横幅（-100）之后，紧邻其他通用项之前。
    order: -90,
    locale: NS,
  }, DefaultDirRow))
}
