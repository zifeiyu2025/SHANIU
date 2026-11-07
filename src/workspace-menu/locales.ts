/** `shaniuWorkspaceMenu` 命名空间：工作区行菜单「在文件夹显示」项的文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuWorkspaceMenu'

/** 简体中文词典（键集的事实来源）。取词与上游 ui-open-in-app 逐字一致。 */
export const zh = {
  'menu.finder': '在 Finder 中显示',
  'menu.reveal': '在文件夹显示',
  'menu.revealFailed': '无法显示文件位置，请重试',
} as const

/** 词典键联合。 */
export type WorkspaceMenuKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<WorkspaceMenuKey, string> = {
  'menu.finder': 'Show in Finder',
  'menu.reveal': 'Show in folder',
  'menu.revealFailed': 'Could not show the file location. Try again.',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 工作区行菜单显示项的完整文案。 */
    shaniuWorkspaceMenu: WorkspaceMenuKey
  }
}
