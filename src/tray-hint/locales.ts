/**
 * 托盘提示词典（只维护中英两语，见 `docs/08` §5.5）。
 */
export const NS = 'shaniuTrayHint' as const

export const zh = {
  trayHint: '仍有 {count} 个终端会话在运行，退出应用才会终止。',
}

export const en: typeof zh = {
  trayHint: 'Still {count} terminal session(s) running; they stop only when the app quits.',
}

export type TrayHintKey = keyof typeof zh

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 托盘提示的文案。 */
    shaniuTrayHint: TrayHintKey
  }
}
