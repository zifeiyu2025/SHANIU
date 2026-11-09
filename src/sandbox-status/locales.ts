/** `shaniuSandbox` 命名空间：沙箱异常提示文案（常态不渲染，故只有异常口径）。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuSandbox'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'notice.partial': '文件限制未全部强制：当前只有逻辑隔离，没有路径级写入边界',
  'error.status': '读取沙箱状态失败：{error}',
  'refresh': '刷新沙箱状态',
} as const

/** 词典键联合。 */
export type SandboxStatusKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<SandboxStatusKey, string> = {
  'notice.partial': 'File boundaries are not fully enforced: logical isolation only, with no path-level write boundary',
  'error.status': 'Could not read sandbox status: {error}',
  'refresh': 'Refresh sandbox status',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 沙箱异常提示的全部文案。 */
    shaniuSandbox: SandboxStatusKey
  }
}
