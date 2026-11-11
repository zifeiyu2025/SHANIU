/** `shaniuAttachmentUsage` 命名空间：通用设置页里「附件占用」一行。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuAttachmentUsage'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'title': '附件占用',
  // 只报数，不提清理：清理是破坏性操作，且这一版没有清理入口。
  'hint': '历史图片与文件存放在数据目录的 attachments 下；这里只显示占用，不提供清理。',
  'value': '{count} 个文件 / {size}',
} as const

/** 英文词典。 */
export const en: Record<keyof typeof zh, string> = {
  'title': 'Attachment storage',
  'hint': 'Images and files are kept under attachments in the data directory. This row only reports usage; it does not clean anything up.',
  'value': '{count} files / {size}',
}

/** 本插件的词典键。 */
export type AttachmentUsageKey = keyof typeof zh

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 通用设置页「附件占用」一行的文案。 */
    shaniuAttachmentUsage: AttachmentUsageKey
  }
}
