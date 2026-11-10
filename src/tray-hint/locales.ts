/**
 * 托盘提示词典（只维护中英两语，见 `docs/08` §5.5）。
 */
export const NS = 'shaniuTrayHint' as const

export const zh = {
  trayHintRunning: 'SHANIU 仍在后台运行，{count} 个终端/任务未结束。要彻底退出：右键托盘图标 → 退出，或会话头「更多操作」→ 退出 SHANIU。',
  trayHintIdle: 'SHANIU 已最小化到托盘，程序仍在运行。要彻底退出：右键托盘图标 → 退出，或会话头「更多操作」→ 退出 SHANIU。',
  trayHintFirst: '托盘图标在任务栏右下角，可能收在「^」里。',
}

export const en: typeof zh = {
  trayHintRunning: 'SHANIU is still running in the background with {count} terminal/task(s) active. To quit: right-click the tray icon → Quit, or the session header “More actions” → Quit SHANIU.',
  trayHintIdle: 'SHANIU was minimized to the tray; the app is still running. To quit: right-click the tray icon → Quit, or the session header “More actions” → Quit SHANIU.',
  trayHintFirst: 'The tray icon is at the bottom-right of the taskbar, possibly inside “^”.',
}

export type TrayHintKey = keyof typeof zh

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 托盘提示的文案。 */
    shaniuTrayHint: TrayHintKey
  }
}
