/** `shaniuQuitGuard` 命名空间：退出确认对话框。 */

export const NS = 'shaniuQuitGuard'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'quit.title': '还有任务在运行',
  'quit.body': '退出会取消这些任务，正在等待的审批也会一并取消。',
  'quit.activeTask': '正在运行的会话：{title}',
  'quit.jobs': '后台作业：{count} 个（{sessions}）',
  'quit.terminals': '后台终端：{count} 个',
  'quit.approvals': '待审批：{count} 项（退出等同于取消）',
  'quit.cancelAndQuit': '取消任务并退出',
  'quit.keepRunning': '继续跑，稍后再退',
  'quit.hideToTray': '收回托盘',
  'quit.cancelling': '正在取消任务…（最多等待 3 秒）',
  'quit.forced': '等待超时，已强制退出并写入记录。',
  'quit.failed': '取消失败：{message}',
} as const

export type QuitGuardKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<QuitGuardKey, string> = {
  'quit.title': 'Tasks are still running',
  'quit.body': 'Quitting cancels these tasks, including any pending approvals.',
  'quit.activeTask': 'Running session: {title}',
  'quit.jobs': 'Background jobs: {count} ({sessions})',
  'quit.terminals': 'Background terminals: {count}',
  'quit.approvals': 'Pending approvals: {count} (quitting cancels them)',
  'quit.cancelAndQuit': 'Cancel tasks and quit',
  'quit.keepRunning': 'Keep running, quit later',
  'quit.hideToTray': 'Minimize to tray',
  'quit.cancelling': 'Cancelling tasks… (waiting up to 3 seconds)',
  'quit.forced': 'Wait timed out; quit was forced and recorded.',
  'quit.failed': 'Cancel failed: {message}',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 退出确认对话框的文案。 */
    shaniuQuitGuard: QuitGuardKey
  }
}
