/** `shaniuSandbox` 命名空间：沙箱异常提示文案（常态不渲染，故只有异常口径）。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuSandbox'

/**
 * 简体中文词典（键集的事实来源）。
 *
 * `notice.preparing` / `notice.needs_admin` 说的是**文件权限准备**，不许出现
 * 实现细节（目录层级、安全描述符、授权对象一类的词）——用户只需要知道
 * 「现在会有点问题 / 怎么办」。
 */
export const zh = {
  'notice.partial': '文件限制未全部强制：当前只有逻辑隔离，没有路径级写入边界',
  'notice.preparing': '正在准备文件权限，刚启动时个别命令可能失败，稍等重试就好。',
  'notice.needs_admin': '有些目录需要管理员权限才能准备好；如果命令提示找不到路径，以管理员身份运行一次 SHANIU 就好。',
  'error.status': '读取沙箱状态失败：{error}',
  'refresh': '刷新沙箱状态',
} as const

/** 词典键联合。 */
export type SandboxStatusKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<SandboxStatusKey, string> = {
  'notice.partial': 'File boundaries are not fully enforced: logical isolation only, with no path-level write boundary',
  'notice.preparing': 'Preparing file permissions. Some commands may fail right after launch; try again in a moment.',
  'notice.needs_admin': 'Some folders need administrator rights to be prepared. If a command reports a missing path, run SHANIU as administrator once.',
  'error.status': 'Could not read sandbox status: {error}',
  'refresh': 'Refresh sandbox status',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 沙箱异常提示的全部文案。 */
    shaniuSandbox: SandboxStatusKey
  }
}
