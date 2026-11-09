/** `shaniuModelCapabilities` 命名空间：设置页服务商卡片里的能力声明 / 实测视图。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuModelCapabilities'

/** 简体中文词典（键集的事实来源）。 */
export const zh = {
  'title': '能力（声明 + 实测）',
  'hint': '实测只判对错，不改声明值',
  'item.vision': '视觉',
  'item.tools': '工具',
  'item.reasoning': '推理档',
  'item.context_window': '上下文',
  'item.max_output': '输出上限',
  'item.builtin_web_search': '托管搜索',
  'source.unknown': '未声明',
  'source.endpoint': '端点',
  'source.builtin': '内置表',
  'source.inferred': '推测',
  'source.probe': '实测',
  'source.manual': '手动',
  'state.verified': '已验证',
  'state.mismatch': '与声明不符',
  'state.probing': '检测中',
  'state.unverified': '未验证',
  'cost': '首次检测约 5 次小请求，消耗少量 token（每模型一次）',
  'costDetail': '视觉 2 次（各含一张 64×64 图）、工具 1 次、推理 2 次（low / max 各一问）。prompt 侧每问几十 token，生成侧受探针上限约束；推理档的思考 token 由模型决定——deepseek-flash 实测两档合计约 250 reasoning token。结果按「端点 + 协议 + 模型 + 密钥指纹」缓存，同一组合只测一次；失败或超时只记「未验证」，不影响模型可用。',
  'value.yes': '有',
  'value.no': '无',
  'value.none': '未声明',
  'dataDate': '内置表数据 {date}',
  'evidence': '证据：{evidence}',
  'loadFailed': '读取能力声明失败：{error}',
} as const

/** 词典键联合。 */
export type CapabilitiesKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<CapabilitiesKey, string> = {
  'title': 'Capabilities (declared + measured)',
  'hint': 'Probes only judge; they never rewrite declarations',
  'item.vision': 'Vision',
  'item.tools': 'Tools',
  'item.reasoning': 'Reasoning',
  'item.context_window': 'Context',
  'item.max_output': 'Max output',
  'item.builtin_web_search': 'Hosted search',
  'source.unknown': 'Undeclared',
  'source.endpoint': 'Endpoint',
  'source.builtin': 'Built-in table',
  'source.inferred': 'Guessed',
  'source.probe': 'Probed',
  'source.manual': 'Manual',
  'state.verified': 'Verified',
  'state.mismatch': 'Declared ≠ measured',
  'state.probing': 'Checking',
  'state.unverified': 'Unverified',
  'cost': 'First check ≈ 5 tiny requests, a little token spend (once per model)',
  'costDetail': 'Vision 2 (each with a 64×64 image), tools 1, reasoning 2 (low / max prompts). Prompts are a few dozen tokens; generation is capped by the probe budget; reasoning tokens are model-dependent — deepseek-flash measured ~250 across both levels. Results are cached per endpoint + protocol + model + key fingerprint; failures or timeouts only mark "unverified" and never disable the model.',
  'value.yes': 'yes',
  'value.no': 'no',
  'value.none': 'undeclared',
  'dataDate': 'Built-in data {date}',
  'evidence': 'Evidence: {evidence}',
  'loadFailed': 'Failed to read capability declarations: {error}',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 设置页模型卡片的能力区块文案。 */
    shaniuModelCapabilities: CapabilitiesKey
  }
}
