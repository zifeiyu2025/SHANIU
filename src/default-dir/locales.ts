/** `shaniuDefaultDir` 命名空间：通用设置页里「默认目录」一行。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuDefaultDir'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'title': '默认目录',
  // 逐条对齐代码事实：没配置工作区 → 默认根就是 `<数据目录>\Workspace` 且在生效，
  // run() 里判等成立才 create_dir_all；配置了合法自定义根 → 生效的是配置值，默认目录不创建。
  // 不要写成「当前未使用」——默认值仍然是它，只是没被启用时不落盘。
  'hint': '没有配置工作区时就用它，并会创建；配置了别的工作区时不会创建。',
  'active': '当前工作区是 {path}（不是默认目录）。',
} as const

/** 英文词典。 */
export const en: Record<keyof typeof zh, string> = {
  'title': 'Default directory',
  'hint': 'It is used and created when no workspace is configured; when another workspace is configured, it is not created.',
  'active': 'The current workspace is {path} (not the default directory).',
}

/** 本插件的词典键。 */
export type DefaultDirKey = keyof typeof zh

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 通用设置页「默认目录」一行的文案。 */
    shaniuDefaultDir: DefaultDirKey
  }
}
