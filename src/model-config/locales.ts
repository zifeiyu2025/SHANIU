/** `shaniuModelConfig` 命名空间：未配置模型时占位席位上的文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuModelConfig'

/** 简体中文词典（键集的事实来源；空态/加载态沿用 DSH ModelSelect 原文）。 */
export const zh = {
  'seat.empty': '请选择模型',
  'seat.loading': '正在加载模型…',
  'seat.aria': '打开设置中的模型页以配置模型',
} as const

/** 词典键联合。 */
export type ModelConfigKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<ModelConfigKey, string> = {
  'seat.empty': 'Select a model',
  'seat.loading': 'Loading models…',
  'seat.aria': 'Open Models settings to configure a model',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 未配置模型时席位上的文案。 */
    shaniuModelConfig: ModelConfigKey
  }
}
