/** `shaniuSandbox` 命名空间：沙箱状态条文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuSandbox'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'label.sandbox': '沙箱',
  'label.mechanism': '机制',
  'mode.read_only': '只读',
  'mode.workspace_write': '工作区可写',
  'mode.danger_full_access': '完全放开',
  'enforcement.full': '强制完整',
  'enforcement.partial': '强制不完整',
  'enforcement.fullHint': '宿主能把该档位声明的文件边界全部强制到位',
  'enforcement.partialHint': '宿主只能强制部分边界（逻辑沙箱没有路径级写入边界）',
  'os.app_container': 'OS 沙箱（AppContainer）',
  'os.logical': '逻辑沙箱',
  'os.logicalWarning': '当前未启用 OS 沙箱，仅受限令牌 + Job 逻辑隔离，没有路径级写入边界',
  'profile': '容器 profile：{profile}',
  'error.status': '读取沙箱状态失败：{error}',
  'refresh': '刷新沙箱状态',
} as const

/** 词典键联合。 */
export type SandboxStatusKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<SandboxStatusKey, string> = {
  'label.sandbox': 'Sandbox',
  'label.mechanism': 'Mechanism',
  'mode.read_only': 'Read-only',
  'mode.workspace_write': 'Workspace write',
  'mode.danger_full_access': 'Full access',
  'enforcement.full': 'Fully enforced',
  'enforcement.partial': 'Partially enforced',
  'enforcement.fullHint': 'The host fully enforces every file boundary this mode declares',
  'enforcement.partialHint': 'The host enforces only part of the boundary (logical sandbox has no path-level write boundary)',
  'os.app_container': 'OS sandbox (AppContainer)',
  'os.logical': 'Logical sandbox',
  'os.logicalWarning': 'OS sandbox is disabled: only restricted token + Job (logical isolation), no path-level write boundary',
  'profile': 'Container profile: {profile}',
  'error.status': 'Could not read sandbox status: {error}',
  'refresh': 'Refresh sandbox status',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 沙箱状态条的全部文案。 */
    shaniuSandbox: SandboxStatusKey
  }
}
