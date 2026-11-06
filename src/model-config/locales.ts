/** `shaniuModelConfig` 命名空间：未配置模型时占位席位上的文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuModelConfig'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'seat.label': '模型配置',
  'seat.aria': '模型配置：打开设置中的模型页',
} as const

/** 词典键联合。 */
export type ModelConfigKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<ModelConfigKey, string> = {
  'seat.label': 'Model setup',
  'seat.aria': 'Model setup: open the Models settings',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 未配置模型时席位上的文案。 */
    shaniuModelConfig: ModelConfigKey
  }
}
