/**
 * 「附件占用」一行（宿主插件）。
 *
 * 挂载点：`settings.general.item`，排在「默认目录」之后。只读展示
 * `attachments/` 的总文件数与总字节数——不提供清理按钮：清理是破坏性操作，
 * 删错了就是「历史图片凭空消失」，先让量级可见。
 */
import type { Context } from '@deepseek-ai/cordis'
import { AttachmentUsageRow } from './AttachmentUsageRow'
import { NS, en, zh } from './locales'

/** 必需服务：槽位与词典。 */
export const inject = ['slots', 'locale']

/**
 * 注册通用设置页的「附件占用」一行。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-attachment-usage: dictionaries')
  ctx.slots.inject('settings.general.item', () => ctx.slots.register({
    name: 'settings.general.item',
    id: 'attachment-usage',
    // 紧跟在「默认目录」(-90) 之后。
    order: -85,
    locale: NS,
  }, AttachmentUsageRow))
}
