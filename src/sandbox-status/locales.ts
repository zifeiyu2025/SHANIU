/** `shaniuSandbox` 命名空间：沙箱状态条与网络授权的文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuSandbox'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'label.sandbox': '沙箱',
  'label.network': '网络',
  'mode.read_only': '只读',
  'mode.workspace_write': '工作区可写',
  'mode.danger_full_access': '完全放开',
  'os.app_container': 'OS 沙箱（AppContainer）',
  'os.logical': '逻辑沙箱',
  'os.logicalWarning': '当前未启用 OS 沙箱，仅受限令牌 + Job 逻辑隔离，没有路径级写入边界',
  'profile': '容器 profile：{profile}',
  'network.denied': '已断（默认拒绝）',
  'network.grantedSessions': '已放行：{sessions}',
  'action.grant': '允许联网',
  'action.revoke': '撤销联网',
  'action.granting': '正在放行…',
  'action.revoking': '正在撤销…',
  'action.grantTooltip': '允许本会话访问网络（会话级一次性授权，写入审计）',
  'action.revokeTooltip': '撤销本会话的网络访问（后续命令立即断网）',
  'feedback.granted': '已允许本会话访问网络（审计已记录）；撤销前该会话内命令都可出站',
  'feedback.revoked': '已撤销网络访问：本会话后续命令将断网',
  'feedback.grantFailed': '放行失败：{error}',
  'feedback.revokeFailed': '撤销失败：{error}',
  'feedback.notGranted': '该会话当前没有网络授权，无需撤销',
  'error.status': '读取沙箱状态失败：{error}',
  'refresh': '刷新沙箱状态',
} as const

/** 词典键联合。 */
export type SandboxStatusKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<SandboxStatusKey, string> = {
  'label.sandbox': 'Sandbox',
  'label.network': 'Network',
  'mode.read_only': 'Read-only',
  'mode.workspace_write': 'Workspace write',
  'mode.danger_full_access': 'Full access',
  'os.app_container': 'OS sandbox (AppContainer)',
  'os.logical': 'Logical sandbox',
  'os.logicalWarning': 'OS sandbox is disabled: only restricted token + Job (logical isolation), no path-level write boundary',
  'profile': 'Container profile: {profile}',
  'network.denied': 'Blocked (default deny)',
  'network.grantedSessions': 'Allowed: {sessions}',
  'action.grant': 'Allow network',
  'action.revoke': 'Revoke network',
  'action.granting': 'Allowing…',
  'action.revoking': 'Revoking…',
  'action.grantTooltip': 'Allow network for this session (one-time, session-scoped, audited)',
  'action.revokeTooltip': 'Revoke network for this session (subsequent commands are offline)',
  'feedback.granted': 'Network allowed for this session (audit recorded); every command in it can reach the network until revoked',
  'feedback.revoked': 'Network access revoked: subsequent commands in this session are offline',
  'feedback.grantFailed': 'Could not allow network: {error}',
  'feedback.revokeFailed': 'Could not revoke network: {error}',
  'feedback.notGranted': 'This session has no network grant to revoke',
  'error.status': 'Could not read sandbox status: {error}',
  'refresh': 'Refresh sandbox status',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 沙箱状态条与网络授权的全部文案。 */
    shaniuSandbox: SandboxStatusKey
  }
}
