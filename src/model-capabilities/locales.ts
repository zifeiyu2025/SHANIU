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
  'state.unjudgeable': '不可判',
  'unjudgeableHint': '该端点无法判定推理档差异，不影响使用。需要时可在上方重新检测。',
  'retry': '重新检测',
  'probingProgress': '检测中 {seconds}s · 预计 10–20 秒',
  'cost': '首次检测 9 次小请求（视觉 2 / 工具 1 / 推理 6），约 10–20 秒（每模型一次）',
  'costDetail': '视觉 2 次（各含一张 256×256 图）、工具 1 次、推理 6 次（low / max 各 3 次交错采样，上限 512 token）。prompt 侧每问几十 token，生成侧受探针上限约束；推理 token 由模型决定——deepseek-flash 实测每次 79–201 completion token，两档合计约 630–800。结果按「端点 + 协议 + 模型 + 密钥指纹 + 探针版本」缓存，同一组合只测一次；失败或超时只记「未验证」，不影响模型可用。判不出差异时如实显示「不可判」，不会为了凑结论硬判。',
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
  'state.unjudgeable': 'Undetermined',
  'unjudgeableHint': 'This endpoint cannot resolve reasoning-level differences; the model is still usable. Re-check above if needed.',
  'retry': 'Re-check',
  'probingProgress': 'Checking {seconds}s · about 10–20s',
  'cost': 'First check = 9 tiny requests (vision 2, tools 1, reasoning 6), about 10–20s (once per model)',
  'costDetail': 'Vision 2 (each with a 256×256 image), tools 1, reasoning 6 (3 per level, interleaved, capped at 512 tokens). Prompts are a few dozen tokens; generation is capped by the probe budget; reasoning tokens are model-dependent — deepseek-flash measured 79–201 completion tokens per call, ~630–800 across both levels. Results are cached per endpoint + protocol + model + key fingerprint + probe version, so a combination is checked once; failures or timeouts only mark "unverified" and never disable the model. If the signal cannot separate levels it reports "undetermined" rather than forcing a verdict.',
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
