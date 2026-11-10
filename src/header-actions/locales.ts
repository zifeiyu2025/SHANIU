/** `shaniuHeader` 命名空间：会话标题行两个入口的文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuHeader'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'explorer.app': '文件资源管理器',
  'explorer.tooltip': '用 {app} 打开',
  'menu.copyMarkdown': '复制会话为 Markdown',
  'menu.copyId': '复制会话 ID',
  'menu.copyFailed': '复制失败，请重试',
  'menu.quit': '退出 SHANIU',
} as const

/** 词典键联合。 */
export type HeaderKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<HeaderKey, string> = {
  'explorer.app': 'File Explorer',
  'explorer.tooltip': 'Open with {app}',
  'menu.copyMarkdown': 'Copy session as Markdown',
  'menu.copyId': 'Copy session ID',
  'menu.copyFailed': 'Could not copy. Try again.',
  'menu.quit': 'Quit SHANIU',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 会话标题行入口的完整文案。 */
    shaniuHeader: HeaderKey
  }
}
